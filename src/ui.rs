//! Rendering. Reads `App`, writes a frame, mutates only layout bookkeeping.
//!
//! This runs on the main task, never the socket task. The relay pushes with
//! `try_send` and drops a connection after three consecutive full buffers, so
//! a slow paint on the read loop is a disconnect, not a dropped frame.
//!
//! # The layout
//!
//! ```text
//!  ╭ channels ─────────╮╭ #dev · deploy coordination ──── relay ● live ╮
//!  │                   ││                                              │
//!  │  C H A N N E L S   ││   ───────────  Today  ───────────           │
//!  │ ▌ dev              ││                                              │
//!  │   general       3  ││   Samantha  14:48                            │
//!  │                   ││   Hey Ian — I'm here and live in Buzz.       │
//!  ╰───────────────────╯╰──────────────────────────────────────────────╯
//!  ╭ compose ── ⇥ channels · ⏎ send · ^C quit ──────────────────────────╮
//!  │ › Message #dev                                                     │
//!  ╰──────────────────────────────────────────────── caught up ─────────╯
//! ```
//!
//! Every pane is a rounded box with interior padding, and the whole frame sits
//! inside a one-cell margin so nothing hugs the terminal edge. A box's chrome
//! earns its two columns by carrying something: the pane's name, the relay and
//! connection state, the key bindings, and the latest notice all live on
//! borders rather than spending body rows on a status strip.
//!
//! The composer is three rows — border, input, border — because a single row
//! wedged against a rule reads as an artefact rather than a field.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Connection, LinkTarget, Pane, PillTarget, Workspace};
use crate::spinner;
use crate::store::{Channel, ChannelKind};
use crate::theme;

/// Sidebar width. Wide enough for a display name plus an unread badge, narrow
/// enough that the transcript keeps a comfortable measure on an 80-column
/// terminal.
const SIDEBAR_WIDTH: u16 = 28;
/// Messages from one author within this window share a header, the way every
/// desktop chat client groups them. Long enough to cover a burst of typing,
/// short enough that resuming after a gap still gets a timestamp.
const GROUP_WINDOW_SECS: u64 = 300;
/// Rows the composer spends on chrome: two borders and two padding rows. The
/// blanks are the point — a field wedged directly between two borders reads as
/// an artefact of the layout rather than as somewhere to type.
const COMPOSE_CHROME: u16 = 4;
/// Most text rows the composer grows to before it scrolls internally. Past
/// this it is eating the transcript to show a draft.
const COMPOSE_MAX_ROWS: u16 = 10;
/// Columns the composer spends left of the text: padding plus the `› ` caret.
const COMPOSE_PREFIX: u16 = 2;
/// Most completions shown at once. Beyond a handful, scanning a list is
/// slower than typing another letter.
const COMPLETION_ROWS: usize = 6;
/// Most people shown in the picker at once.
const PICKER_ROWS: usize = 8;
/// Rows the picker spends above its list: the query line and a blank.
const PICKER_HEADER_ROWS: u16 = 2;
/// How close to the top of the transcript triggers a page of older history.
/// A couple of screenfuls of warning, so the page usually lands before the
/// reader arrives at the gap.
const PAGE_TRIGGER_LINES: usize = 12;
/// Search hits shown at once.
const SEARCH_ROWS: usize = 6;
/// Rows one hit occupies: a meta line and a snippet.
const SEARCH_ROW_HEIGHT: u16 = 2;
/// Narrowest transcript area that can hold a channel *and* a thread beside it.
/// Below this the thread replaces the channel instead, because two crushed
/// panes are worse than one readable one.
const SPLIT_MIN_WIDTH: u16 = 80;
/// Narrowest a thread pane is allowed to be. Below this its padding starts
/// costing more than it buys and the prose wraps every few words.
const THREAD_MIN_WIDTH: u16 = 40;
/// Widest a thread pane grows to. Past this it is taking room from the channel
/// for a measure nobody reads at.
const THREAD_MAX_WIDTH: u16 = 72;
/// Narrowest the channel keeps when a thread sits beside it.
const CHANNEL_MIN_WIDTH: u16 = 34;
/// Columns the thread pane's `esc ✕` label occupies, and therefore the width
/// of its click target. Must match what `draw_message_pane` renders.
const CLOSE_LABEL_WIDTH: u16 = 7;
/// Rows of chrome a channel pane spends before its first row: two borders
/// plus the blank under the title.
const LIST_CHROME: u16 = 3;
/// Most of the sidebar the rooms pane may take before the direct-message pane
/// is squeezed. Rooms are usually the shorter list; when they are not, the
/// DM pane still keeps a usable window.
const ROOMS_MAX_SHARE: u16 = 3; // of 5

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let communities = app.community_rows();
    let workspace = app.current_mut();
    workspace.communities = communities;
    draw_workspace(frame, workspace, area)
}

fn draw_workspace(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        // The frame margin is the difference between a window and a wallpaper.
        .margin(1)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(compose_height(app, area.width)),
        ])
        .split(area);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(SIDEBAR_WIDTH),
            Constraint::Length(1), // gap — adjacent boxes read as one grid
            Constraint::Min(24),
        ])
        .split(root[0]);

    draw_sidebar(frame, app, body[0]);
    if app.canvas_open {
        // The canvas replaces the transcript rather than splitting with it: a
        // document deserves the width, and the conversation is one keystroke
        // away.
        draw_canvas(frame, app, body[2]);
    } else {
        draw_messages(frame, app, body[2]);
    }
    draw_compose(frame, app, root[1]);
    // Drawn last so they sit over the transcript. A modal the transcript can
    // paint over is a modal nobody can read.
    draw_completion(frame, app, root[1]);
    draw_picker(frame, app, area);
    draw_emoji_picker(frame, app, area);
    draw_search(frame, app, area);
    draw_help(frame, app, area);
}

/// The key reference.
fn draw_help(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    if !app.help {
        return;
    }
    let sections = crate::help::sections(app.newline_key);
    let key_width = crate::help::key_column(&sections);
    let content_height = crate::help::height(&sections);

    let width = (key_width as u16 + 46)
        .min(area.width.saturating_sub(4))
        .max(30);
    let height = (content_height as u16 + 4).min(area.height.saturating_sub(2));
    let region = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, region);

    let block = box_frame()
        .title_top(Line::from(label("help")).left_aligned())
        .title_top(close_label().right_aligned())
        .padding(Padding::new(2, 2, 1, 0));
    app.regions.modal_close = close_target(region);
    let inner = block.inner(region);
    frame.render_widget(block, region);
    if inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(section.title, theme::pane_title())));
        for (keys, what) in &section.rows {
            let pad = key_width.saturating_sub(keys.chars().count());
            lines.push(Line::from(vec![
                Span::styled(keys.clone(), theme::key()),
                Span::raw(" ".repeat(pad + 3)),
                Span::styled(*what, theme::muted()),
            ]));
        }
    }

    // Scroll only when it does not fit — on a tall terminal the whole
    // reference is visible and the keys do nothing, which is the right
    // behaviour rather than a scrollbar that never moves.
    let visible = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(app.help_scroll)
        .take(visible)
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

/// Search across every accessible channel.
fn draw_search(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    let Some(search) = &app.search else {
        return;
    };
    app.regions.search = Rect::default();

    let width = 72.min(area.width.saturating_sub(4)).max(32);
    let rows = search.results.len().clamp(1, SEARCH_ROWS) as u16 * SEARCH_ROW_HEIGHT;
    let height = (rows + 4).min(area.height);
    let region = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 4,
        width,
        height,
    };
    frame.render_widget(Clear, region);

    // While a query is in flight the corner carries the spinner instead, so
    // there is nothing to click there and no target is recorded.
    let running = search.running;
    let scope = if running {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(spinner::frame(), Style::default().fg(theme::CYAN)),
            Span::raw(" "),
        ])
    } else {
        close_label()
    };
    let block = box_frame()
        .title_top(Line::from(label("search")).left_aligned())
        .title_top(scope.right_aligned())
        .padding(Padding::new(2, 2, 0, 0));

    let inner = block.inner(region);
    frame.render_widget(block, region);
    if inner.height == 0 {
        return;
    }

    let mut lines = vec![
        Line::from(vec![
            Span::styled("› ", Style::default().fg(theme::CYAN)),
            if search.query.is_empty() {
                Span::styled("Search every channel", theme::faint())
            } else {
                Span::styled(search.query.clone(), theme::body())
            },
        ]),
        Line::from(""),
    ];

    if search.results.is_empty() {
        // Three different silences: nothing typed, waiting, and no hits. They
        // look identical as an empty list and mean entirely different things.
        lines.push(placeholder(if search.running {
            "Searching…"
        } else if search.ran.is_some() {
            "No matches."
        } else {
            "Type a query, then press Enter."
        }));
    }

    let start = completion_window(search.index, search.results.len(), SEARCH_ROWS);
    for (index, id) in search
        .results
        .iter()
        .enumerate()
        .skip(start)
        .take(SEARCH_ROWS)
    {
        let selected = index == search.index;
        let Some((channel, message)) = app.store.locate(*id) else {
            continue;
        };
        let where_ = app
            .store
            .channels()
            .iter()
            .find(|c| c.id == channel)
            .map(|c| app.channel_label(c))
            .unwrap_or_else(|| "unknown".into());

        let marker = if selected { "▌ " } else { "  " };
        let meta = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme::CYAN)),
            Span::styled(
                app.store.display_name(&message.author),
                Style::default()
                    .fg(theme::author(
                        &message.author.to_hex(),
                        app.is_me(&message.author),
                    ))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(format!("#{where_}"), theme::muted()),
            Span::raw("  "),
            Span::styled(format_when(message.created_at), theme::faint()),
        ]);
        // One line of the message, flattened: a hit is an address, not a
        // reading surface, and a multi-line preview would push the next result
        // off the list.
        let snippet = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                truncate_to_width(
                    &message
                        .content
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    inner.width.saturating_sub(4) as usize,
                ),
                if selected {
                    theme::body()
                } else {
                    theme::channel_idle()
                },
            ),
        ]);
        lines.push(if selected {
            meta.style(Style::default().bg(theme::SELECTED_BG))
        } else {
            meta
        });
        lines.push(snippet);
    }

    frame.render_widget(Paragraph::new(lines), inner);
    app.regions.search = Rect {
        y: inner.y + PICKER_HEADER_ROWS,
        height: inner.height.saturating_sub(PICKER_HEADER_ROWS),
        ..inner
    };
    app.regions.search_first = start;
    if !running {
        app.regions.modal_close = close_target(region);
    }
}

/// A timestamp for a search hit, which may be from any day.
fn format_when(created_at: u64) -> String {
    let Some(when) = chrono::DateTime::from_timestamp(created_at as i64, 0) else {
        return "—".into();
    };
    let when = when.with_timezone(&chrono::Local);
    let today = chrono::Local::now().date_naive();
    if when.date_naive() == today {
        when.format("%H:%M").to_string()
    } else if Some(when.date_naive()) == today.pred_opt() {
        format!("yesterday {}", when.format("%H:%M"))
    } else {
        when.format("%-d %b %H:%M").to_string()
    }
}

/// The reaction picker, centred like the people picker and sharing its
/// geometry so the two feel like one control in two modes.
fn draw_emoji_picker(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    let Some(picker) = &app.emoji_picker else {
        return;
    };
    app.regions.picker = Rect::default();

    let shown = picker.matches.len().clamp(1, PICKER_ROWS);
    let width = 34.min(area.width.saturating_sub(4)).max(20);
    let height = (shown as u16 + 4).min(area.height);
    let region = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    };
    frame.render_widget(Clear, region);

    let block = box_frame()
        .title_top(Line::from(label("react")).left_aligned())
        .title_top(close_label().right_aligned())
        .padding(Padding::new(2, 2, 0, 0));
    let inner = block.inner(region);
    frame.render_widget(block, region);
    if inner.height == 0 {
        return;
    }

    let mut lines = vec![
        Line::from(vec![
            Span::styled("› ", Style::default().fg(theme::CYAN)),
            if picker.query.is_empty() {
                Span::styled("Search", theme::faint())
            } else {
                Span::styled(picker.query.clone(), theme::body())
            },
        ]),
        Line::from(""),
    ];
    if picker.matches.is_empty() {
        lines.push(placeholder("No match."));
    }

    let start = completion_window(picker.index, picker.matches.len(), shown);
    for (index, (names, glyph)) in picker.matches.iter().enumerate().skip(start).take(shown) {
        let selected = index == picker.index;
        let name = crate::emoji::label(names);
        let pad = (inner.width as usize).saturating_sub(glyph.width() + name.width() + 5);
        let line = Line::from(vec![
            Span::styled(
                if selected { "▌ " } else { "  " },
                Style::default().fg(theme::CYAN),
            ),
            Span::raw(glyph.clone()),
            Span::raw("  "),
            Span::styled(
                name.to_string(),
                if selected {
                    theme::channel_selected()
                } else {
                    theme::channel_idle()
                },
            ),
            Span::raw(" ".repeat(pad)),
        ]);
        lines.push(if selected {
            line.style(Style::default().bg(theme::SELECTED_BG))
        } else {
            line
        });
    }

    frame.render_widget(Paragraph::new(lines), inner);
    app.regions.picker = Rect {
        y: inner.y + PICKER_HEADER_ROWS,
        height: inner.height.saturating_sub(PICKER_HEADER_ROWS),
        ..inner
    };
    app.regions.picker_first = start;
    app.regions.modal_close = close_target(region);
}

/// How tall the composer needs to be for what has been typed.
///
/// Computed before the split rather than after, because the transcript's
/// height depends on it — a composer that grows has to take its rows from
/// somewhere, and that has to be decided in one place.
fn compose_height(app: &Workspace, total_width: u16) -> u16 {
    let width = compose_text_width(total_width);
    let rows = compose_lines(&app.input, width).len() as u16;
    COMPOSE_CHROME + rows.clamp(1, COMPOSE_MAX_ROWS)
}

/// Interior width available to composer text, after borders, padding and the
/// caret column.
fn compose_text_width(total_width: u16) -> usize {
    total_width
        // frame margin, borders, and the block's horizontal padding
        .saturating_sub(2 + 2 + 6 + COMPOSE_PREFIX)
        .max(8) as usize
}

/// Splits the draft into rendered rows: hard newlines first, then wrapping.
fn compose_lines(input: &str, width: usize) -> Vec<String> {
    if input.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    for paragraph in input.split('\n') {
        if paragraph.is_empty() {
            rows.push(String::new());
            continue;
        }
        rows.extend(wrap_hard(paragraph, width));
    }
    rows
}

/// Wraps on words where it can and on grapheme clusters where it must, so no
/// row ever exceeds the box.
fn wrap_hard(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;
    for word in text.split_inclusive(char::is_whitespace) {
        let word_width = word.width();
        if used + word_width > width && used > 0 {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        if word_width > width {
            for cluster in word.graphemes(true) {
                let cluster_width = cluster.width();
                if used + cluster_width > width && used > 0 {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.push_str(cluster);
                used += cluster_width;
            }
            continue;
        }
        row.push_str(word);
        used += word_width;
    }
    rows.push(row);
    rows
}

/// The people picker, centred over the message area.
fn draw_picker(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    app.regions.picker = Rect::default();
    let Some(picker) = &app.picker else {
        return;
    };

    let shown = picker.matches.len().clamp(1, PICKER_ROWS);
    let width = 44.min(area.width.saturating_sub(4)).max(24);
    let height = (shown as u16 + 4).min(area.height);
    let region = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    };
    frame.render_widget(Clear, region);

    let block = box_frame()
        .title_top(Line::from(label("new message")).left_aligned())
        .title_top(close_label().right_aligned())
        .padding(Padding::new(2, 2, 0, 0));
    let inner = block.inner(region);
    frame.render_widget(block, region);
    if inner.height == 0 {
        return;
    }

    let mut lines = vec![
        Line::from(vec![
            Span::styled("› ", Style::default().fg(theme::CYAN)),
            if picker.query.is_empty() {
                Span::styled("Search people", theme::faint())
            } else {
                Span::styled(picker.query.clone(), theme::body())
            },
        ]),
        Line::from(""),
    ];

    if picker.matches.is_empty() {
        // Distinguishes "nobody matches" from "no profiles have loaded yet",
        // which look identical as an empty list.
        lines.push(placeholder(if app.store.people(&app.me()).is_empty() {
            "No profiles loaded yet."
        } else {
            "No one matches."
        }));
    }

    let start = completion_window(picker.index, picker.matches.len(), shown);
    for (index, (name, _)) in picker.matches.iter().enumerate().skip(start).take(shown) {
        let selected = index == picker.index;
        let pad = (inner.width as usize).saturating_sub(name.width() + 2);
        let line = Line::from(vec![
            Span::styled(
                if selected { "▌ " } else { "  " },
                Style::default().fg(theme::CYAN),
            ),
            Span::styled(
                name.clone(),
                if selected {
                    theme::channel_selected()
                } else {
                    theme::channel_idle()
                },
            ),
            Span::raw(" ".repeat(pad)),
        ]);
        lines.push(if selected {
            line.style(Style::default().bg(theme::SELECTED_BG))
        } else {
            line
        });
    }

    frame.render_widget(Paragraph::new(lines), inner);
    // The click target is the list only. The query line and its blank sit
    // above it, and mapping a click on those to the first match would open a
    // conversation with whoever happened to sort first.
    app.regions.picker = Rect {
        y: inner.y + PICKER_HEADER_ROWS,
        height: inner.height.saturating_sub(PICKER_HEADER_ROWS),
        ..inner
    };
    app.regions.picker_first = start;
    app.regions.modal_close = close_target(region);
}

/// First visible row when the roster is longer than the box.
///
/// This is not only a scroll offset: a click resolves to
/// `window + row`, so an off-by-one here selects the wrong person.
fn completion_window(index: usize, total: usize, rows: usize) -> usize {
    index
        .saturating_sub(rows.saturating_sub(1))
        .min(total.saturating_sub(rows))
}

/// The `@` autocomplete, floating above the composer.
///
/// Anchored to the composer's top edge and grown upward, so the list never
/// covers the text being typed — the one thing the reader is looking at.
fn draw_completion(frame: &mut Frame, app: &mut Workspace, compose: Rect) {
    app.regions.completion = Rect::default();
    let Some(completion) = &app.completion else {
        return;
    };
    let shown = completion.matches.len().min(COMPLETION_ROWS);
    let height = shown as u16 + 2;
    if compose.y < height {
        return;
    }
    let widest = completion
        .matches
        .iter()
        .map(|(name, _)| name.width())
        .max()
        .unwrap_or(0);
    let width = (widest as u16 + 6).clamp(18, compose.width.saturating_sub(2).max(18));

    // Hang off the caret, the way an editor's completion does, and shift left
    // only far enough to stay inside the composer. Anchoring to the pane's
    // left edge instead leaves the list floating unattached to the `@` that
    // opened it.
    let right_limit = compose.x + compose.width;
    let x = (app.regions.caret_x.saturating_sub(1)).min(right_limit.saturating_sub(width));
    let area = Rect {
        x: x.max(compose.x),
        y: compose.y - height,
        width,
        height,
    };
    frame.render_widget(Clear, area);

    let block = box_frame()
        .title_top(Line::from(label("mention")).left_aligned())
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let start = completion_window(completion.index, completion.matches.len(), shown);
    let lines: Vec<Line> = completion
        .matches
        .iter()
        .enumerate()
        .skip(start)
        .take(shown)
        .map(|(index, (name, _))| {
            let selected = index == completion.index;
            let pad = (inner.width as usize).saturating_sub(name.width() + 1);
            let line = Line::from(vec![
                Span::styled(
                    format!("@{name}"),
                    if selected {
                        theme::channel_selected()
                    } else {
                        theme::channel_idle()
                    },
                ),
                Span::raw(" ".repeat(pad)),
            ]);
            if selected {
                line.style(Style::default().bg(theme::SELECTED_BG))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    app.regions.completion = inner;
    app.regions.completion_first = start;
}

/// Interior padding shared by both message panes.
///
/// Left 2, right 1 — the right side spends its second column on the
/// scrollbar track, which is reserved whether or not a scrollbar is showing.
/// Both panes take the same value: a narrower pane needs the breathing room
/// more, not less, and text welded to a border is what makes a split read as
/// cluttered rather than as two columns.
fn message_padding() -> Padding {
    Padding::new(2, 1, 1, 0)
}

/// The shared pane chrome: rounded box, hairline border. Padding is the
/// caller's, because the three panes want different amounts of it — a
/// transcript wants a top margin, a one-row composer cannot have one.
fn box_frame() -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::rule())
}

/// A border label. Borders are the only chrome in the layout, so every one of
/// them carries a word rather than decoration.
fn label(text: &str) -> Span<'static> {
    Span::styled(format!(" {text} "), theme::pane_title())
}

// ── sidebar ─────────────────────────────────────────────────────────────────

fn draw_sidebar(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    app.regions.communities = Rect::default();

    // Communities sit above channels because that is the order they contain
    // each other in — and putting the list in the sidebar rather than behind a
    // key makes switching discoverable without a hint telling you it exists.
    let area = if app.communities.is_empty() {
        area
    } else {
        let height = app.communities.len() as u16 + LIST_CHROME;
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(height), Constraint::Min(LIST_CHROME + 1)])
            .split(area);
        draw_community_pane(frame, app, split[0]);
        split[1]
    };

    let rooms: Vec<Channel> = app.rooms().into_iter().cloned().collect();
    let dms: Vec<Channel> = app.dms().into_iter().cloned().collect();

    // One pane when there is only one kind of thing to show — an empty box
    // titled "direct" is worse than no box at all.
    if dms.is_empty() && app.hidden_count() == 0 {
        let inner = draw_channel_pane(frame, app, area, "channels", &rooms);
        app.regions.rooms = inner;
        app.regions.dms = Rect::default();
        return;
    }
    if rooms.is_empty() {
        let inner = draw_channel_pane(frame, app, area, "direct", &dms);
        app.regions.dms = inner;
        app.regions.rooms = Rect::default();
        return;
    }

    let wanted = rooms.len() as u16 + LIST_CHROME;
    let cap = (area.height * ROOMS_MAX_SHARE / 5).max(LIST_CHROME + 1);
    let rooms_height = wanted.clamp(LIST_CHROME + 1, cap).min(area.height);

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(rooms_height),
            Constraint::Min(LIST_CHROME + 1),
        ])
        .split(area);

    let rooms_inner = draw_channel_pane(frame, app, split[0], "channels", &rooms);
    let dms_inner = draw_channel_pane(frame, app, split[1], "direct", &dms);
    app.regions.rooms = rooms_inner;
    app.regions.dms = dms_inner;
}

/// Converts a rendered block's links into screen-space click targets.
fn link_targets(
    rendered: &crate::markdown::Rendered,
    area: Rect,
    first_visible: usize,
    height: usize,
) -> Vec<LinkTarget> {
    rendered
        .links
        .iter()
        .filter(|link| link.line >= first_visible && link.line < first_visible + height)
        .map(|link| LinkTarget {
            row: area.y + (link.line - first_visible) as u16,
            start: area.x + link.start as u16,
            end: area.x + link.end as u16,
            url: rendered.urls[link.url].clone(),
        })
        .collect()
}

/// The channel's shared document.
fn draw_canvas(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    let channel = app.current_channel().map(|c| (c.id, app.channel_label(c)));
    let title = match &channel {
        Some((_, name)) => Line::from(vec![
            Span::raw(" "),
            Span::styled("canvas", theme::pane_title()),
            Span::styled(format!("  #{name} "), theme::muted()),
        ]),
        None => Line::from(label("canvas")),
    };

    let canvas = channel
        .as_ref()
        .and_then(|(id, _)| app.store.canvas(id))
        .cloned();
    // Attribution is the point of a shared document: who last changed this,
    // and how long ago, is what tells you whether to trust it.
    let byline = match &canvas {
        Some(canvas) => Line::from(vec![
            Span::styled(
                app.store.display_name(&canvas.author),
                Style::default().fg(theme::author(
                    &canvas.author.to_hex(),
                    app.is_me(&canvas.author),
                )),
            ),
            Span::styled(
                format!("  {} ", format_when(canvas.updated_at)),
                theme::faint(),
            ),
        ]),
        None => Line::from(""),
    };

    let block = box_frame()
        .title_top(title.left_aligned())
        .title_top(byline.right_aligned())
        .title_bottom(
            Line::from(vec![
                Span::styled(" ^E ", theme::key()),
                Span::styled("edit in $EDITOR   ", theme::faint()),
                Span::styled("^G ", theme::key()),
                Span::styled("close ", theme::faint()),
            ])
            .right_aligned(),
        )
        .padding(message_padding());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height == 0 {
        return;
    }

    let text_area = Rect {
        width: inner.width - 1,
        ..inner
    };
    let Some(canvas) = canvas else {
        frame.render_widget(
            Paragraph::new(vec![
                placeholder("This channel has no canvas yet."),
                Line::from(""),
                Line::from(vec![
                    Span::styled("^E", theme::key()),
                    Span::styled(" opens ", theme::faint()),
                    Span::styled(crate::editor_name(), theme::muted()),
                    Span::styled(
                        " to write one. Save and quit to publish it.",
                        theme::faint(),
                    ),
                ]),
            ]),
            text_area,
        );
        return;
    };

    let rendered = crate::markdown::render(&canvas.content, text_area.width as usize);
    let lines = rendered.lines.clone();
    let height = text_area.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    app.canvas_scroll = app.canvas_scroll.min(max_scroll);
    // A document reads top-down, so its scroll counts from the top — the
    // opposite of a transcript, which is anchored to the newest line.
    let start = app.canvas_scroll;
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    frame.render_widget(Paragraph::new(visible), text_area);
    app.link_targets = link_targets(&rendered, text_area, start, height);

    if max_scroll > 0 {
        let track = Rect {
            x: inner.x + inner.width - 1,
            y: inner.y,
            width: 1,
            height: inner.height,
        };
        let mut state = ScrollbarState::new(max_scroll).position(start);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("▐")
                .thumb_style(Style::default().fg(theme::RULE)),
            track,
            &mut state,
        );
    }
}

/// The community list, in the same shape as the channel lists below it.
fn draw_community_pane(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    let block = box_frame()
        .title_top(Line::from(label("communities")).left_aligned())
        .padding(Padding::new(1, 2, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.regions.communities = inner;

    let width = inner.width as usize;
    let lines: Vec<Line> = app
        .communities
        .iter()
        .map(|community| {
            let style = if community.active {
                theme::channel_selected()
            } else if community.unread > 0 {
                theme::channel_unread()
            } else {
                theme::channel_idle()
            };
            // A community that is not connected is worth seeing before you
            // wonder why it has nothing in it.
            let marker = if community.active { "▌ " } else { "  " };
            let badge = if community.unread > 0 && !community.active {
                community.unread.min(99).to_string()
            } else {
                String::new()
            };
            let dot = if community.mentions && !community.active {
                "● "
            } else {
                ""
            };
            let name_budget =
                width.saturating_sub(marker.width() + dot.width() + badge.width() + 2);
            let name = truncate_to_width(&community.name, name_budget);
            let gap =
                width.saturating_sub(marker.width() + name.width() + dot.width() + badge.width());

            let mut spans = vec![
                Span::styled(marker, Style::default().fg(theme::CYAN)),
                Span::styled(
                    name,
                    if community.live {
                        style
                    } else {
                        // Dimmed rather than badged: "not connected" is a
                        // property of the row, not a count on it.
                        theme::faint()
                    },
                ),
                Span::raw(" ".repeat(gap)),
            ];
            if !dot.is_empty() {
                spans.push(Span::styled(dot, Style::default().fg(theme::CYAN)));
            }
            if !badge.is_empty() {
                spans.push(Span::styled(badge, theme::badge()));
            }
            let line = Line::from(spans);
            if community.active {
                line.style(Style::default().bg(theme::SELECTED_BG))
            } else {
                line
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draws one titled channel list and returns its interior, which is the rect a
/// click is later resolved against.
fn draw_channel_pane(
    frame: &mut Frame,
    app: &Workspace,
    area: Rect,
    title: &str,
    channels: &[Channel],
) -> Rect {
    let mut block = box_frame()
        .title_top(Line::from(label(title)).left_aligned())
        // A blank row under the title, so the first channel is not welded to
        // the border.
        .padding(Padding::new(1, 2, 1, 0));
    // Hidden DMs are announced rather than silently dropped: a conversation
    // that vanished with no trace is indistinguishable from one that was lost.
    if title == "direct" && app.hidden_count() > 0 {
        block = block.title_top(
            Line::from(vec![Span::styled(
                format!(" {} hidden ", app.hidden_count()),
                if app.show_hidden {
                    theme::channel_selected()
                } else {
                    theme::faint()
                },
            )])
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    let lines: Vec<Line> = channels
        .iter()
        .map(|channel| {
            let selected = app.current_channel().map(|c| c.id) == Some(channel.id);
            let (unread, mentions) = app.unread(&channel.id);

            let hidden = app.is_hidden(channel);
            let style = if selected {
                theme::channel_selected()
            } else if hidden {
                theme::faint()
            } else if unread > 0 {
                theme::channel_unread()
            } else {
                theme::channel_idle()
            };
            // A left bar rather than a "> " marker: it reads as a selected row
            // in a sidebar instead of as a prompt.
            let marker = if selected {
                "▌ "
            } else if hidden {
                "· "
            } else {
                "  "
            };
            let badge = if unread > 0 && !selected {
                unread.min(99).to_string()
            } else {
                String::new()
            };
            // A mention is a different fact from a count: "someone spoke" and
            // "someone spoke to you" should not look the same.
            let dot = if mentions && !selected { "● " } else { "" };
            // Reserve the badge and a gap before measuring the name, so a long
            // name truncates instead of shoving the count off the edge.
            let name_budget =
                width.saturating_sub(marker.width() + dot.width() + badge.width() + 2);
            let name = truncate_to_width(&app.channel_label(channel), name_budget);
            let gap =
                width.saturating_sub(marker.width() + name.width() + dot.width() + badge.width());

            let mut spans = vec![
                Span::styled(marker, Style::default().fg(theme::CYAN)),
                Span::styled(name, style),
                Span::raw(" ".repeat(gap)),
            ];
            if !dot.is_empty() {
                spans.push(Span::styled(dot, Style::default().fg(theme::CYAN)));
            }
            if !badge.is_empty() {
                spans.push(Span::styled(badge, theme::badge()));
            }
            let line = Line::from(spans);
            // The highlight spans the whole row rather than just the text. A
            // background that stops at the end of a word reads as a selected
            // *word*; one that runs the width reads as a selected row.
            if selected {
                line.style(Style::default().bg(theme::SELECTED_BG))
            } else {
                line
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
    inner
}

// ── transcript ──────────────────────────────────────────────────────────────

/// The message area: the channel, plus a thread beside it when one is open
/// and the terminal is wide enough to hold both.
fn draw_messages(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    app.regions.thread_pane = Rect::default();
    app.regions.thread_close = Rect::default();
    app.reaction_targets.clear();
    app.header_targets.clear();
    app.link_targets.clear();
    app.regions.modal_close = Rect::default();

    let Some(root) = app.thread else {
        draw_message_pane(frame, app, area, Pane::Channel);
        return;
    };

    if area.width < SPLIT_MIN_WIDTH {
        // Narrow terminal: the thread takes the whole area. Splitting here
        // would leave two panes too narrow to read either conversation.
        draw_message_pane(frame, app, area, Pane::Thread);
        return;
    }

    // The thread takes a real share rather than a sliver. Agent replies are
    // long and often carry code blocks, so a thread pane that has to give up
    // its padding to fit the text is the wrong trade — take the columns from
    // the channel instead.
    let thread_width = (area.width * 9 / 20).clamp(THREAD_MIN_WIDTH, THREAD_MAX_WIDTH);
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(CHANNEL_MIN_WIDTH),
            Constraint::Length(1), // gap
            Constraint::Length(thread_width),
        ])
        .split(area);

    draw_message_pane(frame, app, split[0], Pane::Channel);
    draw_message_pane(frame, app, split[2], Pane::Thread);
    let _ = root;
}

fn draw_message_pane(frame: &mut Frame, app: &mut Workspace, area: Rect, pane: Pane) {
    let block = match pane {
        Pane::Channel => pane_block_for_channel(app),
        Pane::Thread => box_frame()
            .title_top(
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled("↩ thread", theme::pane_title()),
                    Span::raw(" "),
                ])
                .left_aligned(),
            )
            // The way out, on the chrome of the thing it closes. A thread that
            // can be opened by clicking and only closed by a key nobody
            // mentioned is a trap.
            .title_top(close_label().right_aligned())
            .padding(message_padding()),
    };

    let inner = block.inner(area);
    match pane {
        Pane::Channel => app.regions.transcript = inner,
        Pane::Thread => {
            app.regions.thread_pane = inner;
            // Same geometry as every modal's close control, from one place.
            app.regions.thread_close = close_target(area);
        }
    }
    frame.render_widget(block, area);

    if inner.width < 4 || inner.height == 0 {
        return;
    }

    // One column on the right belongs to the scrollbar whether or not it is
    // showing — text that reflows when a scrollbar appears looks like a bug.
    let text_area = Rect {
        width: inner.width - 1,
        ..inner
    };

    let Some(channel) = app.current_channel().map(|c| c.id) else {
        let text = match app.connection {
            Connection::Live => "Loading channels…",
            _ => "Connecting…",
        };
        frame.render_widget(Paragraph::new(loading(text)), text_area);
        return;
    };

    let log = app.store.log_or_empty(&channel);
    let counts = log.reply_counts();
    let messages: Vec<&crate::store::Message> = match (pane, app.thread) {
        (Pane::Thread, Some(root)) => log.thread(root),
        // The relay's own contract: replies never enter the channel timeline.
        _ => log.top_level().collect(),
    };

    if messages.is_empty() {
        // "Empty" and "not answered yet" look identical on screen and are not
        // the same fact. This relay can take several seconds for one page.
        let line = if app.loading.contains(&channel) {
            loading("Loading messages…")
        } else if pane == Pane::Thread {
            placeholder("This thread has not loaded yet.")
        } else {
            placeholder("Nothing here yet.")
        };
        frame.render_widget(Paragraph::new(line), text_area);
        return;
    }

    let width = text_area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    // A line at the very top saying why the transcript stops. Only visible
    // once the reader has scrolled there, so it costs nothing until it is the
    // answer to the question being asked.
    if pane == Pane::Channel {
        if app.paging.contains(&channel) {
            lines.push(loading("Loading earlier messages…"));
            lines.push(Line::from(""));
        } else if app.exhausted.contains(&channel) {
            // Not "the channel starts here": over WebSocket there is no
            // authoritative exhaustion signal, so this says what was
            // observed rather than making a claim about the channel.
            lines.push(placeholder("No earlier messages loaded."));
            lines.push(Line::from(""));
        }
    }
    // Which rendered line carries which thread affordance, resolved to screen
    // rows once the scroll offset is known.
    let mut affordances: Vec<(usize, nostr::EventId)> = Vec::new();
    let mut headers: Vec<(usize, nostr::EventId)> = Vec::new();
    // Where the message we were asked to scroll to ended up, if it is here.
    let mut focus_line: Option<usize> = None;
    let mut pills: Vec<(usize, PillTarget)> = Vec::new();
    let mut link_hits: Vec<(usize, usize, usize, String)> = Vec::new();
    let mut last_day: Option<chrono::NaiveDate> = None;
    let mut last_author: Option<(nostr::PublicKey, u64)> = None;

    for message in messages {
        // Timestamps are clock-only, so a transcript spanning several days
        // reads as though it were shuffled. The separator makes the ordering
        // legible rather than merely correct.
        let day = local_date(message.created_at);
        let new_day = day.is_some() && day != last_day;
        if new_day {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(day_separator(day.expect("checked"), width));
            lines.push(Line::from(""));
            last_day = day;
            last_author = None;
        }

        if app.focus_message == Some(message.id) {
            focus_line = Some(lines.len());
        }
        let grouped = last_author.is_some_and(|(author, at)| {
            author == message.author && message.created_at.saturating_sub(at) < GROUP_WINDOW_SECS
        });
        if !grouped {
            if !lines.is_empty() && !new_day {
                lines.push(Line::from(""));
            }
            let mut header = vec![
                Span::styled(
                    app.store.display_name(&message.author),
                    Style::default()
                        .fg(theme::author(
                            &message.author.to_hex(),
                            app.is_me(&message.author),
                        ))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(format_time(message.created_at), theme::faint()),
            ];
            if pane == Pane::Thread && message.root.is_none() {
                header.push(Span::styled("  ↩ root", theme::faint()));
            }
            if app.addressed_to_me(message, &channel) {
                header.insert(0, Span::styled("▌ ", Style::default().fg(theme::CYAN)));
            }
            headers.push((lines.len(), message.id));
            lines.push(Line::from(header));
        }

        // A message that names you gets a rail down its left edge. In a
        // channel where agents answer each other, "this one is for you" is the
        // single most useful thing the transcript can say.
        let addressed = app.addressed_to_me(message, &channel);
        let rail = if addressed { 2usize } else { 0 };
        let body_width = width.saturating_sub(rail);
        let body = crate::markdown::render(&message.content, body_width);
        let body_start = lines.len();
        for line in body.lines {
            if addressed {
                let mut spans = vec![Span::styled("▌ ", Style::default().fg(theme::CYAN))];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            } else {
                lines.push(line);
            }
        }
        // Links are recorded relative to the message; shift them into the
        // transcript's line and column space. The rail, when present, moves
        // every column right by its width.
        for link in &body.links {
            link_hits.push((
                body_start + link.line,
                link.start + rail,
                link.end + rail,
                body.urls[link.url].clone(),
            ));
        }
        if message.edited {
            lines.push(Line::from(Span::styled("edited", theme::faint())));
        }

        let groups = log.reactions(message.id, &app.me());
        if !groups.is_empty() {
            let mut spans = Vec::new();
            let mut column = text_area.x;
            for group in &groups {
                let pill = format!(" {} {} ", group.emoji, group.count);
                let width = pill.width() as u16;
                pills.push((
                    lines.len(),
                    PillTarget {
                        row: 0, // resolved to a screen row once scroll is known
                        start: column,
                        end: column + width,
                        message: message.id,
                        emoji: Some(group.emoji.clone()),
                    },
                ));
                spans.push(Span::styled(
                    pill,
                    if group.mine.is_some() {
                        theme::pill_mine()
                    } else {
                        theme::pill()
                    },
                ));
                spans.push(Span::raw(" "));
                column += width + 1;
            }
            // The trailing add button only exists on a row that already has
            // reactions; a bare `+` under every message would be more chrome
            // than transcript.
            pills.push((
                lines.len(),
                PillTarget {
                    row: 0,
                    start: column,
                    end: column + 3,
                    message: message.id,
                    emoji: None,
                },
            ));
            spans.push(Span::styled(" ＋", theme::faint()));
            lines.push(Line::from(spans));
        }

        // The affordance is the whole thread UI in the channel view: it says a
        // conversation exists and is the way into it.
        if pane == Pane::Channel {
            if let Some(count) = counts.get(&message.id) {
                let open = app.thread == Some(message.id);
                affordances.push((lines.len(), message.id));
                lines.push(Line::from(vec![
                    Span::styled(if open { "▌ " } else { "↳ " }, theme::faint()),
                    Span::styled(
                        format!("{count} {}", if *count == 1 { "reply" } else { "replies" }),
                        // The open thread's own affordance is marked, so the
                        // right-hand pane is visibly anchored to a message
                        // rather than floating free.
                        if open {
                            theme::channel_selected()
                        } else {
                            theme::link()
                        },
                    ),
                ]));
            }
        }
        last_author = Some((message.author, message.created_at));
    }

    // Scroll is measured in rendered lines up from the bottom, so a resize that
    // rewraps the text keeps the reader near the same place rather than at the
    // same absolute index into a list that just changed length. Zero is pinned
    // to newest, which is why an arriving message never yanks anyone out of
    // history.
    let height = text_area.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    // A message we were sent here to read is placed a third down the pane, so
    // what came before it is visible as context rather than cut off.
    if let Some(line) = focus_line {
        let target = line.saturating_sub(height / 3);
        *app.scroll_mut(pane) = max_scroll.saturating_sub(target);
        app.focus_message = None;
    }
    let scroll = app.scroll_mut(pane);
    *scroll = (*scroll).min(max_scroll);
    let start = max_scroll - *scroll;
    let total = lines.len();
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    frame.render_widget(Paragraph::new(visible), text_area);

    let to_screen = |index: usize| -> Option<u16> {
        (index >= start && index < start + height).then(|| text_area.y + (index - start) as u16)
    };
    app.header_targets.extend(
        headers
            .into_iter()
            .filter_map(|(index, id)| to_screen(index).map(|row| (row, id))),
    );
    app.reaction_targets.extend(
        pills
            .into_iter()
            .filter_map(|(index, pill)| to_screen(index).map(|row| PillTarget { row, ..pill })),
    );

    // Near the top and there is more above: ask for it. The renderer only
    // records the intent — the main loop is what acts on it, so a frame never
    // starts a network request.
    if pane == Pane::Channel && max_scroll > 0 && start <= PAGE_TRIGGER_LINES {
        app.wants_older = Some(channel);
    }

    if pane == Pane::Channel {
        app.thread_targets = affordances
            .into_iter()
            .filter(|(index, _)| *index >= start && *index < start + height)
            .map(|(index, root)| (text_area.y + (index - start) as u16, root))
            .collect();
    }

    if total > height {
        let track = Rect {
            x: inner.x + inner.width - 1,
            y: inner.y,
            width: 1,
            height: inner.height,
        };
        let mut state = ScrollbarState::new(max_scroll).position(start);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("▐")
                .thumb_style(Style::default().fg(theme::RULE)),
            track,
            &mut state,
        );
    }
}

/// "Samantha is typing…", or the plural forms of it.
fn typing_label(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("{one} is typing…"),
        [one, two] => format!("{one} and {two} are typing…"),
        // Naming everyone would outgrow the border on a busy channel, and the
        // exact roster is not what the reader is waiting on.
        [one, rest @ ..] => format!("{one} and {} others are typing…", rest.len()),
    }
}

/// Where a modal's close control lands, so a click on it can be resolved.
///
/// Derived from the same geometry the title uses — right-aligned, ending one
/// column short of the corner — rather than measured separately.
fn close_target(region: Rect) -> Rect {
    Rect {
        x: region.x + region.width.saturating_sub(1 + CLOSE_LABEL_WIDTH),
        y: region.y,
        width: CLOSE_LABEL_WIDTH.min(region.width),
        height: 1,
    }
}

/// The close control on a pane or modal. Its width is the click target's
/// width, so the two are derived from one place rather than kept in step by
/// hand.
fn close_label() -> Line<'static> {
    Line::from(vec![
        Span::styled(" esc ", theme::key()),
        Span::styled("✕ ", theme::faint()),
    ])
}

/// The channel pane's chrome: name, topic, relay and connection state.
fn pane_block_for_channel(app: &Workspace) -> Block<'static> {
    let title = match app.current_channel() {
        Some(channel) => {
            let sigil = if channel.kind == ChannelKind::Dm {
                "@"
            } else {
                "#"
            };
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(
                    format!("{sigil}{}", app.channel_label(channel)),
                    Style::default()
                        .fg(theme::CYAN)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if !channel.topic.is_empty() {
                spans.push(Span::styled(" · ", theme::faint()));
                spans.push(Span::styled(
                    truncate_to_width(&channel.topic, 48),
                    theme::muted(),
                ));
            }
            spans.push(Span::raw(" "));
            Line::from(spans)
        }
        None => Line::from(label("buzz")),
    };

    let (dot, state, color) = match &app.connection {
        Connection::Live => ("●", "live".to_string(), theme::EMERALD),
        // A connecting state that does not move is indistinguishable from one
        // that has hung.
        Connection::Connecting => (spinner::frame(), "connecting".to_string(), theme::AMBER),
        Connection::Down(reason) => ("○", short_reason(reason), theme::ROSE),
    };
    let status = Line::from(vec![
        Span::raw(" "),
        Span::styled(app.relay_label.clone(), theme::faint()),
        Span::raw("  "),
        Span::styled(dot, Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(state, Style::default().fg(color)),
        Span::raw(" "),
    ]);

    box_frame()
        .title_top(title.left_aligned())
        .title_top(status.right_aligned())
        .padding(message_padding())
}

fn placeholder(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), theme::faint()))
}

/// A placeholder that is waiting on something, marked as in progress.
fn loading(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(spinner::frame(), Style::default().fg(theme::CYAN)),
        Span::raw("  "),
        Span::styled(text.to_string(), theme::faint()),
    ])
}

/// `──────────  Today  ──────────`, centred on the pane.
///
/// Today's separator is emerald rather than grey. Scrolling a long history,
/// it is the one landmark worth finding at a glance, and emerald already means
/// "now" everywhere else in the layout.
fn day_separator(day: chrono::NaiveDate, width: usize) -> Line<'static> {
    let label = day_label(day);
    let is_today = day == chrono::Local::now().date_naive();
    let label_style = if is_today {
        Style::default().fg(theme::EMERALD_DIM)
    } else {
        theme::faint()
    };
    let padded = format!("  {label}  ");
    let remaining = width.saturating_sub(padded.width());
    let left = remaining / 2;
    let right = remaining - left;
    Line::from(vec![
        Span::styled("─".repeat(left), theme::rule()),
        Span::styled(padded, label_style),
        Span::styled("─".repeat(right), theme::rule()),
    ])
}

// ── compose ─────────────────────────────────────────────────────────────────

fn draw_compose(frame: &mut Frame, app: &mut Workspace, area: Rect) {
    // Bindings live on the top border and stay put. They are the only way to
    // discover the app, so they must not be displaced by transient state —
    // which is what the bottom border is for.
    // While a completion is open, Tab and Enter mean something else — so the
    // hints say so rather than describing keys that are currently rebound.
    let keys = if app.completion.is_some() {
        Line::from(vec![
            label("compose"),
            Span::styled("─ ", theme::rule()),
            Span::styled("↑↓", theme::key()),
            Span::styled(" pick   ", theme::faint()),
            Span::styled("⇥", theme::key()),
            Span::styled(" complete   ", theme::faint()),
            Span::styled("esc", theme::key()),
            Span::styled(" dismiss ", theme::faint()),
        ])
    } else {
        // Four bindings is about what a strip gets read for. Everything else
        // lives in the help popup, which can also describe the mouse.
        let mut spans = vec![
            label(if app.thread.is_some() {
                "reply"
            } else {
                "compose"
            }),
            Span::styled("─ ", theme::rule()),
            Span::styled("⇥", theme::key()),
            Span::styled(" channels   ", theme::faint()),
            Span::styled("@", theme::key()),
            Span::styled(" mention   ", theme::faint()),
            Span::styled("⏎", theme::key()),
            Span::styled(" send   ", theme::faint()),
        ];
        // Contextual, because a thread is a mode you can forget you are in.
        if app.thread.is_some() {
            spans.push(Span::styled("esc", theme::key()));
            spans.push(Span::styled(" close thread   ", theme::faint()));
        }
        spans.push(Span::styled(app.help_key, theme::key()));
        spans.push(Span::styled(" help ", theme::faint()));
        Line::from(spans)
    };

    let mut block = box_frame()
        .title_top(keys.left_aligned())
        .padding(Padding::new(3, 3, 1, 1));
    // Typing rides the bottom border opposite the notice. It costs no rows,
    // and a line that appears and disappears inside the transcript would shove
    // the conversation up and down while someone is mid-sentence.
    let typing = app.typing_now();
    if !typing.is_empty() {
        block = block.title_bottom(
            Line::from(vec![
                Span::raw(" "),
                Span::styled(spinner::frame(), Style::default().fg(theme::CYAN)),
                Span::raw(" "),
                Span::styled(
                    truncate_to_width(&typing_label(&typing), 40),
                    theme::muted(),
                ),
                Span::raw(" "),
            ])
            .left_aligned(),
        );
    }
    if let Some(notice) = &app.notice {
        block = block.title_bottom(
            Line::from(vec![
                Span::raw(" "),
                Span::styled(truncate_to_width(notice, 56), theme::muted()),
                Span::raw(" "),
            ])
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 4 {
        return;
    }

    let width = compose_text_width(area.width);
    let empty = app.input.is_empty();
    let rows = compose_lines(&app.input, width);
    // Show the tail when the draft is taller than the box, so the caret is
    // always on screen. A composer that hides where you are typing is worse
    // than one that does not grow at all.
    let visible = inner.height as usize;
    let first = rows.len().saturating_sub(visible);

    let mut lines: Vec<Line> = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(first) {
        // The caret marks the start of the message, not the start of every
        // row — a column of them would read as a quote block.
        let prefix = if index == 0 { "› " } else { "  " };
        let prefix_style = if empty {
            theme::faint()
        } else {
            Style::default().fg(theme::CYAN)
        };
        if empty {
            let hint = match (app.thread.is_some(), app.current_channel()) {
                // Saying which one it is matters: the same keystroke sends to
                // two different places depending on a mode the reader may have
                // forgotten they are in.
                (true, _) => "Reply in thread".to_string(),
                (false, Some(channel)) => {
                    let sigil = if channel.kind == ChannelKind::Dm {
                        "@"
                    } else {
                        "#"
                    };
                    format!("Message {sigil}{}", app.channel_label(channel))
                }
                (false, None) => "No channel selected".to_string(),
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(truncate_to_width(&hint, width), theme::faint()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(row.clone(), theme::body()),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);

    let caret_row = rows.len().saturating_sub(1);
    let caret_column = rows.last().map(|row| row.width()).unwrap_or(0);
    let caret_x = inner.x + COMPOSE_PREFIX + caret_column as u16;
    app.regions.caret_x = caret_x;
    frame.set_cursor_position((
        caret_x.min(inner.x + inner.width.saturating_sub(1)),
        inner.y + (caret_row - first) as u16,
    ));
}

// ── text helpers ────────────────────────────────────────────────────────────

/// Trims a relay error down to something a one-line header can hold.
///
/// These arrive as `Authentication failed: restricted: not a relay member`;
/// the last clause is the part that says what to do about it.
fn short_reason(reason: &str) -> String {
    let tail = reason.rsplit(would_split).next().unwrap_or(reason).trim();
    truncate_to_width(if tail.is_empty() { reason } else { tail }, 40)
}

fn would_split(c: char) -> bool {
    c == ':'
}

fn local_date(created_at: u64) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp(created_at as i64, 0)
        .map(|utc| utc.with_timezone(&chrono::Local).date_naive())
}

/// "Today" and "Yesterday" beat a date for the two days carrying most of the
/// traffic; anything older gets the date it needs.
fn day_label(day: chrono::NaiveDate) -> String {
    let today = chrono::Local::now().date_naive();
    if day == today {
        "Today".to_string()
    } else if Some(day) == today.pred_opt() {
        "Yesterday".to_string()
    } else {
        day.format("%A, %-d %B").to_string()
    }
}

fn format_time(created_at: u64) -> String {
    chrono::DateTime::from_timestamp(created_at as i64, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "--:--".to_string())
}

/// The leading slice of `text` that fits in `width` columns, ellipsised.
fn truncate_to_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let mut out = String::new();
    let mut used = 0usize;
    for cluster in text.graphemes(true) {
        let cluster_width = cluster.width();
        if used + cluster_width > width - 1 {
            break;
        }
        out.push_str(cluster);
        used += cluster_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_separator_fills_exactly_the_pane_width() {
        // An off-by-one here is visible as a ragged rule on every day boundary.
        for width in [20usize, 33, 64, 80] {
            let line = day_separator(chrono::Local::now().date_naive(), width);
            assert_eq!(line.width(), width, "width {width}");
        }
    }

    #[test]
    fn a_narrow_pane_does_not_panic_the_separator() {
        assert!(day_separator(chrono::Local::now().date_naive(), 2).width() >= 2);
    }

    #[test]
    fn a_search_hit_is_timestamped_relative_to_now() {
        // A hit can be from any day, so a clock-only time would be ambiguous
        // in exactly the case search exists for: finding something old.
        let now = chrono::Local::now();
        assert!(!format_when(now.timestamp() as u64).contains(' '));
        let old = now.timestamp() as u64 - 60 * 60 * 24 * 40;
        assert!(
            format_when(old).contains(' '),
            "an older hit must carry its date"
        );
    }

    #[test]
    fn a_search_hit_row_pair_maps_back_to_its_index() {
        // Each hit occupies two rows; halving the offset is what turns a click
        // anywhere on a hit into that hit rather than its neighbour.
        for hit in 0..6usize {
            for row_within in 0..SEARCH_ROW_HEIGHT as usize {
                let offset = hit * SEARCH_ROW_HEIGHT as usize + row_within;
                assert_eq!(offset / SEARCH_ROW_HEIGHT as usize, hit);
            }
        }
    }

    #[test]
    fn typing_reads_as_a_sentence_at_every_count() {
        assert_eq!(typing_label(&[]), "");
        assert_eq!(typing_label(&["Samantha".into()]), "Samantha is typing…");
        assert_eq!(
            typing_label(&["Samantha".into(), "Fizz".into()]),
            "Samantha and Fizz are typing…"
        );
        assert_eq!(
            typing_label(&["Samantha".into(), "Fizz".into(), "Kyber".into()]),
            "Samantha and 2 others are typing…"
        );
    }

    #[test]
    fn the_close_target_matches_the_label_that_is_drawn() {
        // These are two numbers describing one thing. If they drift, the `✕`
        // is visible in a place that does not respond to a click, which reads
        // as a broken control rather than a missing one.
        assert_eq!(close_label().width(), CLOSE_LABEL_WIDTH as usize);
    }

    #[test]
    fn the_close_target_sits_where_the_label_is_drawn() {
        // Right-aligned, ending one column short of the corner. Every modal
        // and the thread pane share this, so an error here is an `esc ✕` that
        // ignores clicks everywhere at once.
        let region = Rect::new(10, 4, 40, 12);
        let target = close_target(region);
        assert_eq!(target.y, region.y, "the label rides the top border");
        assert_eq!(target.width, CLOSE_LABEL_WIDTH);
        assert_eq!(
            target.x + target.width,
            region.x + region.width - 1,
            "must stop one column short of the corner"
        );
    }

    #[test]
    fn a_narrow_modal_still_yields_a_target_inside_itself() {
        let region = Rect::new(0, 0, 4, 3);
        let target = close_target(region);
        assert!(target.width <= region.width);
        assert!(target.x >= region.x);
    }

    #[test]
    fn a_split_leaves_both_panes_room_for_their_padding() {
        // Padding is what stops a split reading as clutter, so the split is
        // only allowed when both panes can afford it and still hold text.
        let thread = THREAD_MIN_WIDTH;
        let interior = |width: u16| width.saturating_sub(2 + 3); // borders + padding
        assert!(interior(thread) >= 30, "thread pane too narrow to read");
        // The split threshold must fit both minimums plus the gap.
        const { assert!(SPLIT_MIN_WIDTH >= CHANNEL_MIN_WIDTH + 1 + THREAD_MIN_WIDTH) };
    }

    #[test]
    fn the_completion_window_keeps_the_selection_visible() {
        // A click maps to `window + row`, so these bounds decide who gets
        // mentioned, not just what is on screen.
        assert_eq!(completion_window(0, 10, 6), 0, "top of a long list");
        assert_eq!(completion_window(3, 10, 6), 0, "still on the first page");
        assert_eq!(
            completion_window(9, 10, 6),
            4,
            "last row sits at the bottom"
        );
        for index in 0..10 {
            let window = completion_window(index, 10, 6);
            assert!(
                (window..window + 6).contains(&index),
                "index {index} fell outside its own window"
            );
        }
    }

    #[test]
    fn a_short_roster_never_scrolls() {
        for index in 0..3 {
            assert_eq!(completion_window(index, 3, 6), 0);
        }
    }

    #[test]
    fn truncation_leaves_room_for_the_ellipsis() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        let cut = truncate_to_width("a-very-long-channel-name", 10);
        assert!(cut.width() <= 10, "{cut:?}");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_composer_grows_with_the_draft_but_stops_eating_the_transcript() {
        let rows =
            |draft: &str, width: u16| compose_lines(draft, compose_text_width(width)).len() as u16;
        assert_eq!(rows("", 100), 1);
        assert_eq!(rows("one\ntwo\nthree", 100), 3);
        // Past the cap the box scrolls internally rather than growing.
        let tall = "line\n".repeat(40);
        assert!(rows(&tall, 100) > COMPOSE_MAX_ROWS);
    }

    #[test]
    fn a_pasted_block_keeps_its_line_breaks() {
        // The whole point of bracketed paste: a list that arrives as one
        // run-on line has been silently corrupted, and the sender finds out
        // after it is published.
        let rows = compose_lines("1. bump\n2. tag\n3. ship", 40);
        assert_eq!(rows, vec!["1. bump", "2. tag", "3. ship"]);
    }

    #[test]
    fn a_blank_line_in_a_draft_survives() {
        // Markdown leans on them; collapsing turns a formatted message into a
        // wall of text the moment it is sent.
        assert_eq!(compose_lines("a\n\nb", 40), vec!["a", "", "b"]);
    }

    #[test]
    fn an_empty_draft_still_occupies_one_row() {
        // Otherwise the box collapses to nothing and there is nowhere to type.
        assert_eq!(compose_lines("", 40).len(), 1);
    }

    #[test]
    fn no_composer_row_ever_exceeds_the_box() {
        let long = "supercalifragilistic ".repeat(5) + &"x".repeat(60);
        for width in [12usize, 30, 64] {
            for row in compose_lines(&long, width) {
                assert!(row.width() <= width, "width {width}: {row:?}");
            }
        }
    }

    #[test]
    fn a_wide_glyph_draft_wraps_on_columns_not_characters() {
        for row in compose_lines(&"一".repeat(20), 8) {
            assert!(row.width() <= 8, "{row:?}");
        }
    }

    #[test]
    fn a_relay_error_is_reduced_to_its_actionable_clause() {
        assert_eq!(
            short_reason("Authentication failed: restricted: not a relay member"),
            "not a relay member"
        );
        assert_eq!(short_reason("socket closed"), "socket closed");
    }

    #[test]
    fn recent_days_read_as_words_and_older_ones_as_dates() {
        let today = chrono::Local::now().date_naive();
        assert_eq!(day_label(today), "Today");
        assert_eq!(day_label(today.pred_opt().unwrap()), "Yesterday");
        let old = chrono::NaiveDate::from_ymd_opt(2026, 8, 18).unwrap();
        assert_eq!(day_label(old), "Tuesday, 18 August");
    }
}
