//! `buzz-tui` — a terminal client for a Buzz relay.
//!
//! Configuration matches `buzz-cli` so the two share one setup:
//!
//! ```text
//! BUZZ_RELAY_URL     Relay base URL   [default: http://localhost:3000]
//! BUZZ_PRIVATE_KEY   Nostr secret key, hex or nsec  [required]
//! ```
//!
//! The three tasks are deliberately separate: the socket loop in
//! [`session`] never renders, relay queries are spawned so they cannot stall a
//! keystroke, and this loop only draws and dispatches.

mod app;
mod config;
mod emoji;
mod help;
mod identity;
mod markdown;
mod readstate;
mod session;
mod spinner;
mod store;
mod theme;
mod ui;

use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream, KeyCode,
    KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use nostr::Keys;
use tokio::sync::mpsc;

use crate::app::{App, Connection, Task, Workspace, WorkspaceConfig};
use crate::session::{RelaySession, SessionEvent};

const DEFAULT_RELAY: &str = "http://localhost:3000";
/// How far PageUp/PageDown move. A screenful minus context, in the absence of
/// a viewport height at the point the key is handled.
const PAGE_LINES: usize = 20;
/// Lines per wheel notch. Three is what most terminals and editors use, and a
/// wheel that moves a different distance here than everywhere else feels
/// broken even when it is deliberate.
const WHEEL_LINES: usize = 3;
/// Presence heartbeat period. The relay expires presence after 180s, and its
/// own tests pin that as "three one-minute heartbeat windows".
const PRESENCE_INTERVAL: Duration = Duration::from_secs(60);
/// How long one heartbeat may wait for its OK. Short relative to the interval,
/// so a stalled publish never overlaps the next tick.
const PRESENCE_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the read frontier is published, if it moved.
const READ_STATE_FLUSH: Duration = Duration::from_secs(15);
/// Repaint cadence for state that expires on a clock rather than on an event.
///
/// A typing indicator has no "stopped" event — it ages out — so without a tick
/// the last one would sit on screen until something else happened to redraw.
/// Matches `TYPING_PRUNE_INTERVAL_MS` in the desktop client.
const IDLE_TICK: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> Result<()> {
    // Install ring as the process-level rustls CryptoProvider, matching
    // buzz-cli: a build that unifies both ring and aws-lc-rs features leaves
    // rustls unable to auto-select one, and it panics on first TLS use.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--keychain-import") {
        return identity::import();
    }
    if args
        .iter()
        .any(|arg| arg == "--keychain-import-from-desktop")
    {
        return identity::import_from_desktop();
    }
    if args.iter().any(|arg| arg == "--keychain-delete") {
        return identity::delete();
    }
    let probe = args.iter().any(|arg| arg == "--probe");

    // Community management, before anything connects.
    if let Some(url) = flag_value(&args, "--add-community") {
        return print_communities(config::add(
            &url,
            std::env::var("BUZZ_RELAY_URL").ok().as_deref(),
        )?);
    }
    if let Some(url) = flag_value(&args, "--remove-community") {
        return print_communities(config::remove(&url)?);
    }
    if let Some(url) = flag_value(&args, "--name-community") {
        let name = flag_value_at(&args, "--name-community", 2).unwrap_or_default();
        return print_communities(config::set_name(&url, &name)?);
    }
    if args.iter().any(|arg| arg == "--communities") {
        return print_communities(relay_urls());
    }

    // Probe the terminal now: after the subcommands that run with piped stdin
    // and must not have it touched, and before any of our own machinery reads
    // input — see `keyboard_enhanced`.
    let _ = keyboard_enhanced();

    let relays = relay_urls();
    // Carry an existing single-relay setup onto disk, so it survives the next
    // shell without anyone having to do anything.
    config::seed_if_empty(&relays);
    let (keys, key_source) = identity::load()?;
    let auth_tag = load_auth_tag(&keys)?;

    // One channel for every workspace's session events and background tasks,
    // each tagged with the relay it came from. Routing by URL rather than by
    // index means a reordered list cannot deliver one community's events to
    // another.
    let (task_tx, mut task_rx) = mpsc::channel::<(String, Task)>(128);
    let (event_tx, mut session_rx) = mpsc::channel::<(String, SessionEvent)>(512);

    let mut workspaces = Vec::new();
    let mut sessions = Vec::new();
    for relay in &relays {
        let ws = session::ws_url(relay);
        let (session, mut events) = RelaySession::start(ws.clone(), keys.clone(), auth_tag.clone());
        sessions.push(std::sync::Arc::clone(&session));

        // Forward this session's events into the shared stream.
        let forward = event_tx.clone();
        let tag = ws.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if forward.send((tag.clone(), event)).await.is_err() {
                    return;
                }
            }
        });

        let tagged = TaggedTasks {
            inner: task_tx.clone(),
            workspace: ws.clone(),
        };
        workspaces.push(Workspace::new(
            WorkspaceConfig {
                relay_label: short_host(&ws),
                name: display_name(relay, &ws),
                url: ws,
                newline_key: newline_key(),
                help_key: help_key(),
            },
            keys.clone(),
            std::sync::Arc::clone(&session),
            tagged.into_sender(),
        ));
    }
    drop(event_tx);
    let mut app = App::new(workspaces);

    if probe {
        println!("key     from {}", key_source.as_str());
        println!(
            "config   {} communit{}",
            relays.len(),
            if relays.len() == 1 { "y" } else { "ies" }
        );
        for relay in &relays {
            println!(
                "         {:<18} {relay}",
                display_name(relay, &session::ws_url(relay))
            );
        }
        let ws_urls: Vec<String> = relays.iter().map(|relay| session::ws_url(relay)).collect();
        let outcome = probe_relays(&sessions, &ws_urls, &mut session_rx, &keys).await;
        for session in &sessions {
            session.shutdown();
        }
        return outcome;
    }

    // Presence is ephemeral with a 180-second TTL at the relay, so it has to be
    // republished or the reader silently drops off everyone else's roster.
    for session in &sessions {
        spawn_presence_heartbeat(std::sync::Arc::clone(session), keys.clone());
    }

    let mut terminal = ratatui::init();
    enter_raw_extras();
    let outcome = run(&mut terminal, &mut app, &mut session_rx, &mut task_rx).await;
    // One last flush per community, so quitting right after reading does not
    // throw that away. Bounded, because a hung relay must not hold the
    // terminal in raw mode.
    for workspace in &mut app.workspaces {
        workspace.flush_read_state();
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), tokio::task::yield_now()).await;
    leave_raw_extras();
    ratatui::restore();
    for session in &sessions {
        session.shutdown();
    }
    outcome
}

/// Whether this terminal can report Shift+Enter as distinct from Enter.
///
/// Without the keyboard protocol both send the same byte, so a Shift+Enter
/// binding would silently do nothing — and telling the user to press it would
/// be worse than offering a key that works.
///
/// **Answered once, and only when a terminal session is actually starting.**
/// The probe writes to the terminal and waits for a reply, so running it in a
/// subcommand fed by a pipe — `--keychain-import` reading a key from stdin —
/// is at best pointless and at worst eats the input. The
/// underlying probe writes a query to the terminal and waits for the reply on
/// stdin; once the app's own key-event stream is running it consumes that
/// reply first and the probe times out to `false`. That made the *exit* path
/// disagree with the entry path — flags pushed on the way in were never popped
/// on the way out, so control keys kept arriving as `CSI u` sequences in
/// whatever ran next. `^X` in an editor became "Unknown Command", and quitting
/// left the shell in the same state.
fn keyboard_enhanced() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false))
}

/// The newline key this terminal can actually deliver.
fn newline_key() -> &'static str {
    if keyboard_enhanced() {
        "⇧⏎"
    } else {
        // Alt+Enter arrives as ESC-prefixed and is distinguishable everywhere.
        "⌥⏎"
    }
}

/// Opens a canvas in `$EDITOR`, then offers the result back for publishing.
///
/// The terminal has to be given away completely and taken back afterwards:
/// raw mode, the alternate screen, mouse reporting and the cursor style are
/// all ours, and an editor that inherits them paints into a screen it does not
/// control.
fn edit_canvas(
    terminal: &mut ratatui::DefaultTerminal,
    workspace: &mut Workspace,
    edit: crate::app::CanvasEdit,
) -> Result<()> {
    let (editor, chosen_for_you) = pick_editor();

    // A stable path per channel, so a refused save can be recovered by hand
    // and a second attempt reuses the same draft.
    let path = std::env::temp_dir().join(format!("buzz-canvas-{}.md", edit.channel));
    std::fs::write(&path, &edit.content).context("writing the canvas draft")?;

    leave_raw_extras();
    ratatui::restore();

    let status = std::process::Command::new(&editor).arg(&path).status();

    let mut restored = ratatui::init();
    enter_raw_extras();
    std::mem::swap(terminal, &mut restored);
    // A stale frame from before the editor would otherwise persist until the
    // next event.
    terminal.clear()?;

    match status {
        Ok(status) if status.success() => {
            if chosen_for_you {
                workspace.notice = Some(format!("used {editor} — set $EDITOR to choose your own"));
            }
        }
        Ok(_) => {
            workspace.notice = Some(format!("{editor} exited without saving"));
            return Ok(());
        }
        Err(error) => {
            workspace.notice = Some(format!("could not run {editor}: {error}"));
            return Ok(());
        }
    }

    let content = std::fs::read_to_string(&path).context("reading the canvas draft")?;
    workspace.save_canvas(edit, content, &path.to_string_lossy());
    Ok(())
}

/// The editor to open a canvas in, and whether the choice was made for the
/// user.
///
/// `$VISUAL` then `$EDITOR` are the conventions and are honoured first. With
/// neither set the fallback prefers `nano` over `vi`: dropping someone who
/// never asked for an editor into a modal one with no on-screen way out is a
/// trap, and `nano` prints its own key hints. `vi` remains the last resort
/// because POSIX guarantees it exists.
fn pick_editor() -> (String, bool) {
    for key in ["VISUAL", "EDITOR"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return (value, false);
            }
        }
    }
    for candidate in ["nano", "vi"] {
        if which(candidate) {
            return (candidate.to_string(), true);
        }
    }
    ("vi".to_string(), true)
}

/// The editor name to show in the UI, so the reader knows what is about to
/// open before it takes over the screen.
pub fn editor_name() -> String {
    pick_editor().0
}

/// Whether a command exists on `PATH`.
fn which(command: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(command).is_file())
}

/// Relay URLs to connect to, in order.
///
/// `BUZZ_RELAY_URL` accepts a comma-separated list, because a community *is*
/// its relay URL — the relay resolves which community a request belongs to
/// from the host, before authentication, and fails closed on one it does not
/// recognise. There is no server-side "list my communities" to ask for; the
/// desktop client keeps its own list too.
fn relay_urls() -> Vec<String> {
    let env = std::env::var("BUZZ_RELAY_URL").ok();
    let urls = config::communities(env.as_deref());
    if urls.is_empty() {
        vec![DEFAULT_RELAY.to_string()]
    } else {
        urls
    }
}

/// The value following `flag`, for the community-management options.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    flag_value_at(args, flag, 1)
}

/// The `offset`-th value after `flag`, for options taking more than one.
fn flag_value_at(args: &[String], flag: &str, offset: usize) -> Option<String> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.get(index + offset).cloned()
}

fn print_communities(urls: Vec<String>) -> Result<()> {
    if urls.is_empty() {
        println!("no communities configured");
        return Ok(());
    }
    for url in &urls {
        println!("{:<18} {url}", display_name(url, &session::ws_url(url)));
    }
    Ok(())
}

/// A community's display name.
///
/// A name the user set, if there is one. Otherwise the first label of the
/// relay host — a guess, but a serviceable one, since every Buzz deployment
/// answers NIP-11 with the same generic `"Buzz Relay"` and there is no
/// community name on the wire to read.
fn display_name(url: &str, ws: &str) -> String {
    config::name_for(url).unwrap_or_else(|| community_name(ws))
}

/// The first label of a relay host.
fn community_name(url: &str) -> String {
    let host = short_host(url);
    let first = host.split('.').next().unwrap_or(&host);
    if first.is_empty() || first.chars().all(|c| c.is_ascii_digit() || c == ':') {
        host
    } else {
        first.to_string()
    }
}

/// Wraps the shared task channel so a workspace can send without knowing it is
/// one of several.
struct TaggedTasks {
    inner: mpsc::Sender<(String, Task)>,
    workspace: String,
}

impl TaggedTasks {
    fn into_sender(self) -> mpsc::Sender<Task> {
        let (tx, mut rx) = mpsc::channel::<Task>(64);
        tokio::spawn(async move {
            while let Some(task) = rx.recv().await {
                if self
                    .inner
                    .send((self.workspace.clone(), task))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        tx
    }
}

/// The help key this terminal can actually deliver.
///
/// Ctrl-H and Backspace are the same byte (0x08) without the keyboard
/// protocol, so on those terminals binding Ctrl-H would swallow Backspace and
/// break the composer. F1 works everywhere.
fn help_key() -> &'static str {
    if keyboard_enhanced() {
        "^H"
    } else {
        "F1"
    }
}

/// Turns on mouse reporting and a bar cursor, and makes sure a panic turns
/// both back off.
///
/// The bar is not decoration. The default block cursor inverts the cell it
/// sits on, so parked at the start of an empty composer it blacks out the
/// first letter of the placeholder — the field reads as though it already
/// contains a stray character.
///
/// `ratatui::init` installs a panic hook that restores the screen, but it
/// knows nothing about either of these. Without chaining onto it, a crash
/// leaves the terminal emitting escape sequences for every mouse move — the
/// shell appears to be typing on its own, and the user has to `reset`.
fn enter_raw_extras() {
    let _ = execute!(
        std::io::stdout(),
        EnableMouseCapture,
        // Bracketed paste is what keeps a pasted block multi-line: without it
        // the terminal delivers the newlines as Enter keypresses, which send
        // the message one line at a time.
        EnableBracketedPaste,
        SetCursorStyle::SteadyBar
    );
    if keyboard_enhanced() {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            SetCursorStyle::DefaultUserShape
        );
        previous(info);
    }));
}

fn leave_raw_extras() {
    if keyboard_enhanced() {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        SetCursorStyle::DefaultUserShape
    );
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    session_rx: &mut mpsc::Receiver<(String, SessionEvent)>,
    task_rx: &mut mpsc::Receiver<(String, Task)>,
) -> Result<()> {
    let mut keys = EventStream::new();
    let mut flush = tokio::time::interval(READ_STATE_FLUSH);
    // The first tick fires immediately; `flush_read_state` no-ops on a clean
    // frontier, so startup does not publish an empty blob.
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // A slow relay must not leave a blank screen, so the first paint happens
    // before anything is awaited.
    terminal.draw(|frame| ui::draw(frame, app))?;
    app.current_mut().wants_older = None;

    loop {
        // Fast enough for the spinner while something is animating, idle
        // otherwise. Repainting sixteen times a second forever would be a
        // rude thing for a chat client to do to a laptop.
        let next_tick = tokio::time::Instant::now()
            + if app.current().is_animating() {
                spinner::tick()
            } else {
                IDLE_TICK
            };

        tokio::select! {
            event = keys.next() => match event {
                Some(Ok(event)) => on_terminal_event(app, event),
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
            event = session_rx.recv() => match event {
                Some((workspace, event)) => {
                    if let Some(workspace) = app.workspace_mut(&workspace) {
                        on_session_event(workspace, event);
                    }
                }
                None => break,
            },
            task = task_rx.recv() => match task {
                Some((workspace, task)) => {
                    if let Some(workspace) = app.workspace_mut(&workspace) {
                        workspace.on_task(task);
                    }
                }
                None => break,
            },
            _ = flush.tick() => {
                for workspace in &mut app.workspaces {
                    workspace.flush_read_state();
                }
            }
            // Nothing to do but redraw: the frame below re-evaluates anything
            // that moves or expires with time.
            _ = tokio::time::sleep_until(next_tick) => {}
        }

        if app.should_quit() {
            break;
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        // The renderer records that the reader is near the top; starting the
        // fetch here keeps network work out of the frame.
        if let Some(channel) = app.current_mut().wants_older.take() {
            app.current_mut().page_older(channel);
        }
        // Opening `$EDITOR` means handing the terminal away, which only the
        // owner of the terminal can do — so the renderer asks and this acts.
        if let Some(edit) = app.current_mut().canvas_edit.take() {
            edit_canvas(terminal, app.current_mut(), edit)?;
        }
    }
    Ok(())
}

fn on_terminal_event(app: &mut App, event: TermEvent) {
    // Community selection belongs to the top level; a workspace does not know
    // it is one of several.
    if let TermEvent::Mouse(mouse) = &event {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && app.community_click(mouse.column, mouse.row)
        {
            return;
        }
    }
    if let TermEvent::Key(key) = &event {
        if key.kind == KeyEventKind::Press
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('k')
        {
            app.cycle_workspace();
            return;
        }
    }
    on_workspace_event(app.current_mut(), event)
}

fn on_workspace_event(app: &mut Workspace, event: TermEvent) {
    let key = match event {
        TermEvent::Key(key) => key,
        TermEvent::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => app.click(mouse.column, mouse.row),
                // Scroll what the pointer is over, not what has focus — with a
                // thread open beside the channel there are two scrollable
                // panes, and the wheel must move the one being pointed at.
                MouseEventKind::ScrollUp => {
                    if let Some(pane) = app.pane_at(mouse.column, mouse.row) {
                        app.scroll_up(pane, WHEEL_LINES);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(pane) = app.pane_at(mouse.column, mouse.row) {
                        app.scroll_down(pane, WHEEL_LINES);
                    }
                }
                _ => {}
            }
            return;
        }
        TermEvent::Paste(text) => {
            app.insert_paste(&text);
            return;
        }
        // Resize and focus events need no state change; the redraw after this
        // returns already reflows to the new size.
        _ => return,
    };
    // Windows terminals report press and release; acting on both double-types.
    if key.kind != KeyEventKind::Press {
        return;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Ctrl-C always quits, even mid-completion: a modal that can trap the exit
    // key is a modal that strands people.
    if ctrl && matches!(key.code, KeyCode::Char('c' | 'd')) {
        app.should_quit = true;
        return;
    }

    // Help is outermost: it can be opened from anywhere, and while it is up
    // every key either scrolls it or closes it.
    if app.help {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) => app.close_help(),
            KeyCode::Char('h') if ctrl => app.close_help(),
            KeyCode::Down | KeyCode::Tab => app.help_scroll_by(1),
            KeyCode::Up | KeyCode::BackTab => app.help_scroll_by(-1),
            KeyCode::PageDown => app.help_scroll_by(8),
            KeyCode::PageUp => app.help_scroll_by(-8),
            _ => {}
        }
        return;
    }

    // Search is modal and outermost: it can be opened from anywhere and takes
    // every key until dismissed.
    if app.search.is_some() {
        match key.code {
            KeyCode::Esc => app.dismiss_search(),
            KeyCode::Enter => app.search_submit(),
            KeyCode::Down | KeyCode::Tab => app.search_next(),
            KeyCode::Up | KeyCode::BackTab => app.search_previous(),
            KeyCode::Backspace => app.search_backspace(),
            KeyCode::Char(c) => app.search_input(c),
            _ => {}
        }
        return;
    }

    if app.emoji_picker.is_some() {
        match key.code {
            KeyCode::Esc => app.dismiss_emoji_picker(),
            KeyCode::Enter | KeyCode::Tab => app.emoji_accept(),
            KeyCode::Down => app.emoji_next(),
            KeyCode::Up | KeyCode::BackTab => app.emoji_previous(),
            KeyCode::Backspace => app.emoji_backspace(),
            KeyCode::Char(c) => app.emoji_input(c),
            _ => {}
        }
        return;
    }

    // The picker is modal: it takes every key until it is dismissed, so a
    // keystroke can never fall through and edit the composer behind it.
    if app.picker.is_some() {
        match key.code {
            KeyCode::Esc => app.dismiss_picker(),
            KeyCode::Enter | KeyCode::Tab => app.picker_accept(),
            KeyCode::Down => app.picker_next(),
            KeyCode::Up | KeyCode::BackTab => app.picker_previous(),
            KeyCode::Backspace => app.picker_backspace(),
            KeyCode::Char(c) => app.picker_input(c),
            _ => {}
        }
        return;
    }

    // An open completion takes Tab, Enter and the arrows. Everything else —
    // typing, Backspace — falls through and re-filters it.
    if app.completion.is_some() {
        match key.code {
            // Tab completes rather than cycling. Tab means "finish this word"
            // in every shell and editor, and a Tab that only moves a highlight
            // reads as a broken completion.
            KeyCode::Tab | KeyCode::Enter => return app.accept_completion(),
            KeyCode::Down => return app.completion_next(),
            KeyCode::Up | KeyCode::BackTab => return app.completion_previous(),
            KeyCode::Esc => return app.dismiss_completion(),
            _ => {}
        }
    }

    match key.code {
        // Esc leaves a thread. It does nothing otherwise, which is the point:
        // a key that closes something should never also close the app.
        KeyCode::Esc if app.canvas_open => app.close_canvas(),
        KeyCode::Esc => app.close_thread(),
        KeyCode::Char('t') if ctrl => app.open_newest_thread(),
        // Ctrl-X rather than Ctrl-H: most terminals send Ctrl-H as backspace,
        // so binding it would delete a character instead of hiding a DM.
        KeyCode::Char('x') if ctrl => app.toggle_hide_selected(),
        KeyCode::Char('n') if ctrl => app.open_people_picker(),
        // Contextual: in the canvas this edits the document, in the
        // transcript it reacts to a message.
        KeyCode::Char('e') if ctrl && app.canvas_open => app.request_canvas_edit(),
        KeyCode::Char('e') if ctrl => app.react_to_newest(),
        KeyCode::Char('g') if ctrl => app.toggle_canvas(),
        KeyCode::Char('f') if ctrl => app.open_search(),
        KeyCode::F(1) => app.toggle_help(),
        // Only where the terminal can tell Ctrl-H from Backspace; elsewhere
        // this arm never fires because 0x08 arrives as `Backspace`.
        KeyCode::Char('h') if ctrl => app.toggle_help(),
        KeyCode::Char('r') if ctrl => app.toggle_show_hidden(),
        KeyCode::Tab => app.select_next(),
        KeyCode::BackTab => app.select_previous(),
        KeyCode::PageUp if app.canvas_open => {
            app.canvas_scroll = app.canvas_scroll.saturating_sub(PAGE_LINES)
        }
        KeyCode::PageDown if app.canvas_open => {
            app.canvas_scroll = app.canvas_scroll.saturating_add(PAGE_LINES)
        }
        KeyCode::PageUp => app.scroll_up(app.focused_pane(), PAGE_LINES),
        KeyCode::PageDown => app.scroll_down(app.focused_pane(), PAGE_LINES),
        KeyCode::Up if ctrl => app.scroll_up(app.focused_pane(), 1),
        KeyCode::Down if ctrl => app.scroll_down(app.focused_pane(), 1),
        // Three spellings of the same intent, because no single one works
        // everywhere: Shift+Enter needs the keyboard protocol, Alt+Enter
        // arrives ESC-prefixed, and Ctrl-J is a distinct byte from Ctrl-M.
        KeyCode::Enter
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            app.insert_newline()
        }
        KeyCode::Char('j') if ctrl => app.insert_newline(),
        KeyCode::Enter => app.submit(),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Char('u') if ctrl => app.clear_input(),
        KeyCode::Char('w') if ctrl => app.delete_word(),
        KeyCode::Char(c) => app.insert_char(c),
        _ => {}
    }
}

fn on_session_event(app: &mut Workspace, event: SessionEvent) {
    match event {
        SessionEvent::Connected => {
            app.connection = Connection::Live;
            // Discovery events are stored channel-scoped, so the relay never
            // pushes them to a live subscription. Re-asking on every connect
            // is also what picks up channels added during an outage.
            app.spawn_channel_load();
            app.spawn_read_state();
            app.spawn_visibility();
        }
        SessionEvent::Disconnected(reason) => app.connection = Connection::Down(reason),
        SessionEvent::Event {
            subscription_id,
            event,
        } => app.on_relay_event(&subscription_id, &event),
        SessionEvent::Eose { subscription_id } => app.on_eose(&subscription_id),
        SessionEvent::Notice(message) => app.notice = Some(message),
    }
}

/// Parses the NIP-OA owner attestation from `BUZZ_AUTH_TAG`.
///
/// Verified against our own pubkey before use: an attestation minted for a
/// different key is rejected by the relay anyway, and failing here says so
/// plainly instead of surfacing as a bare `restricted:` at connect time.
fn load_auth_tag(keys: &Keys) -> Result<Option<nostr::Tag>> {
    let Ok(raw) = std::env::var("BUZZ_AUTH_TAG") else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    buzz_sdk::nip_oa::verify_auth_tag(&raw, &keys.public_key())
        .map_err(|error| anyhow::anyhow!("BUZZ_AUTH_TAG does not attest this key: {error}"))?;
    let tag = buzz_sdk::nip_oa::parse_auth_tag(&raw)
        .map_err(|error| anyhow::anyhow!("BUZZ_AUTH_TAG is not a valid NIP-OA tag: {error}"))?;
    Ok(Some(tag))
}

/// Republishes presence every minute — a third of the relay's TTL, so two
/// consecutive failures still leave the reader shown as online.
fn spawn_presence_heartbeat(session: std::sync::Arc<RelaySession>, keys: Keys) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PRESENCE_INTERVAL);
        // `Burst` is the default, and it is wrong here: this loop awaits the
        // relay's OK, so a stalled publish makes the interval "miss" ticks and
        // then fire them back to back. That burst of EVENT frames is exactly
        // what the relay's per-second admission budget rejects — a heartbeat
        // that rate-limits the client it is meant to keep visible.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Ok(builder) = buzz_sdk::build_presence_update("online") else {
                return;
            };
            let Ok(event) = builder.sign_with_keys(&keys) else {
                return;
            };
            // Best effort, and bounded well under the interval so a dead
            // socket cannot hold the loop past its own period. Presence is
            // decoration: a failure here must never surface as an error in a
            // chat client.
            let _ = session.publish(event, PRESENCE_PUBLISH_TIMEOUT).await;
        }
    });
}

/// Connects and reports on every configured community, then walks one of them.
///
/// Auth is reported per community: with more than one configured, a single
/// relay's health says nothing about the one that is actually misbehaving. The
/// detailed walk runs against the first that authenticated, because running it
/// for all of them buries the answer in output.
async fn probe_relays(
    sessions: &[std::sync::Arc<RelaySession>],
    urls: &[String],
    session_rx: &mut mpsc::Receiver<(String, SessionEvent)>,
    keys: &Keys,
) -> Result<()> {
    use buzz_core::kind::{
        KIND_NIP29_GROUP_MEMBERS, KIND_NIP29_GROUP_METADATA, KIND_PROFILE, KIND_STREAM_MESSAGE,
    };
    use std::collections::HashMap;
    use std::time::Instant;

    let me = keys.public_key().to_hex();
    println!("pubkey   {me}");

    let mut pending: Vec<String> = urls.to_vec();
    let mut results: HashMap<String, Result<(), String>> = HashMap::new();
    let _ = tokio::time::timeout(Duration::from_secs(25), async {
        while !pending.is_empty() {
            let Some((url, event)) = session_rx.recv().await else {
                return;
            };
            let outcome = match event {
                SessionEvent::Connected => Ok(()),
                SessionEvent::Disconnected(reason) => Err(reason),
                _ => continue,
            };
            if let Some(index) = pending.iter().position(|it| *it == url) {
                pending.remove(index);
                results.insert(url, outcome);
            }
        }
    })
    .await;

    for url in urls {
        let name = community_name(url);
        match results.get(url) {
            Some(Ok(())) => println!("auth     {name:<18} ok (NIP-42)"),
            Some(Err(reason)) => println!("auth     {name:<18} FAILED: {reason}"),
            None => println!("auth     {name:<18} FAILED: no answer in 25s"),
        }
    }

    let Some(index) = urls
        .iter()
        .position(|url| matches!(results.get(url), Some(Ok(()))))
    else {
        return Ok(());
    };
    let session = &sessions[index];
    println!();
    println!("walking  {}", community_name(&urls[index]));

    let timeout = Duration::from_secs(20);
    macro_rules! step {
        ($label:expr, $filter:expr) => {{
            let started = Instant::now();
            match session.fetch($filter, timeout).await {
                Ok(events) => {
                    println!(
                        "{:<8} {} event(s) in {}ms",
                        $label,
                        events.len(),
                        started.elapsed().as_millis()
                    );
                    events
                }
                Err(error) => {
                    println!(
                        "{:<8} FAILED after {}ms: {error}",
                        $label,
                        started.elapsed().as_millis()
                    );
                    return Ok(());
                }
            }
        }};
    }

    let rosters = step!(
        "member",
        serde_json::json!({"kinds": [KIND_NIP29_GROUP_MEMBERS], "#p": [me], "limit": 500})
    );
    let ids: Vec<String> = rosters
        .iter()
        .filter_map(|event| {
            event.tags.iter().find_map(|tag| {
                let parts = tag.as_slice();
                (parts.first().map(String::as_str) == Some("d"))
                    .then(|| parts.get(1).cloned())
                    .flatten()
            })
        })
        .collect();
    if ids.is_empty() {
        println!("channels none — this pubkey is a member of nothing here");
        return Ok(());
    }

    let metadata = step!(
        "channels",
        serde_json::json!({"kinds": [KIND_NIP29_GROUP_METADATA], "#d": ids, "limit": 500})
    );

    let mut store = crate::store::Store::default();
    for event in rosters.iter().chain(metadata.iter()) {
        store.apply(event);
    }

    // Walk every channel, not just the first: "one channel is empty" and "all
    // of them are" are different bugs.
    for channel in store.channels().to_vec() {
        let history = step!(
            "history",
            serde_json::json!({
                "kinds": [KIND_STREAM_MESSAGE],
                "#h": [channel.id.to_string()],
                "limit": 200
            })
        );
        let mut log = crate::store::Store::default();
        for event in &history {
            log.apply(event);
        }
        let all = log.log_or_empty(&channel.id).messages().len();
        let top = log.log_or_empty(&channel.id).top_level().count();
        println!(
            "         #{:<20} {all} kept, {top} top-level, {} replies",
            channel.name,
            all - top
        );
    }

    let authors = store.all_participants();
    let found = step!(
        "profiles",
        serde_json::json!({"kinds": [KIND_PROFILE], "authors": authors.clone(), "limit": 500})
    );
    println!(
        "         {} of {} participants have a profile",
        found.len(),
        authors.len()
    );
    Ok(())
}

/// Host of the relay URL, for the status line. The full URL crowds out the
/// connection state on a narrow terminal.
fn short_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_is_chosen_by_convention_then_by_fallback() {
        // One test rather than several: `pick_editor` reads process-global
        // environment, and Rust runs tests in the same process in parallel —
        // separate tests mutating VISUAL and EDITOR race each other, which is
        // exactly the flake this replaces.
        std::env::set_var("VISUAL", "hx");
        std::env::set_var("EDITOR", "vim");
        assert_eq!(pick_editor(), ("hx".to_string(), false), "VISUAL leads");

        std::env::remove_var("VISUAL");
        assert_eq!(pick_editor(), ("vim".to_string(), false), "EDITOR follows");

        // An exported-but-blank value would otherwise try to run "".
        std::env::set_var("EDITOR", "   ");
        let (editor, chosen_for_you) = pick_editor();
        assert_ne!(editor, "");
        assert!(chosen_for_you, "a blank value means we still had to pick");

        std::env::remove_var("EDITOR");
        let (editor, chosen_for_you) = pick_editor();
        assert!(chosen_for_you);
        assert!(
            ["nano", "vi"].contains(&editor.as_str()),
            "fallback picked {editor}"
        );
    }

    #[test]
    fn a_flag_value_is_the_argument_after_it() {
        let args: Vec<String> = ["buzz-tui", "--add-community", "https://a.example.com"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            flag_value(&args, "--add-community").as_deref(),
            Some("https://a.example.com")
        );
        assert_eq!(flag_value(&args, "--remove-community"), None);
    }

    #[test]
    fn a_two_value_flag_reads_both_of_its_arguments() {
        let args: Vec<String> = [
            "buzz-tui",
            "--name-community",
            "wss://a.example.com",
            "Kybernesis",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            flag_value(&args, "--name-community").as_deref(),
            Some("wss://a.example.com")
        );
        assert_eq!(
            flag_value_at(&args, "--name-community", 2).as_deref(),
            Some("Kybernesis")
        );
    }

    #[test]
    fn a_flag_with_nothing_after_it_is_not_a_value() {
        // `--add-community` alone must not be read as adding the empty string.
        let args: Vec<String> = ["buzz-tui", "--add-community"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag_value(&args, "--add-community"), None);
    }

    #[test]
    fn a_community_is_named_after_its_subdomain() {
        assert_eq!(
            community_name("wss://kybernesis.communities.buzz.xyz"),
            "kybernesis"
        );
        assert_eq!(community_name("wss://lotf.communities.buzz.xyz"), "lotf");
        // A bare host has no subdomain to take, so the host stands in.
        assert_eq!(community_name("ws://localhost:3000"), "localhost:3000");
    }

    #[test]
    fn the_status_line_shows_a_host_not_a_url() {
        assert_eq!(
            short_host("wss://kybernesis.communities.buzz.xyz"),
            "kybernesis.communities.buzz.xyz"
        );
        assert_eq!(short_host("ws://localhost:3000/"), "localhost:3000");
    }
}
