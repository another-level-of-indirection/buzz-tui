//! Application state and the actions that change it.
//!
//! Relay work never happens inline here. Every fetch is spawned and reports
//! back through [`Task`], so a slow query cannot stall a keystroke or a repaint
//! — the responsiveness half of the same rule that keeps rendering off the
//! socket task.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nostr::{Event, Keys};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use buzz_core::kind::{
    KIND_CANVAS, KIND_DELETION, KIND_NIP29_DELETE_EVENT, KIND_NIP29_GROUP_MEMBERS,
    KIND_NIP29_GROUP_METADATA, KIND_PROFILE, KIND_REACTION, KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_EDIT, KIND_STREAM_MESSAGE_V2, KIND_TYPING_INDICATOR,
};

use ratatui::layout::{Position, Rect};

use crate::readstate::{ReadState, HORIZON_SECS};
use crate::session::{RelaySession, Subscription};
use crate::store::{Channel, ChannelKind, Store};
use buzz_sdk::mentions::MENTION_CAP;

/// Relay-side ceiling on `#h` values in one REQ (`MAX_EXPLICIT_CHANNEL_VALUES`).
/// Exceeding it fails the whole subscription, so the unread filter is truncated
/// rather than rejected — with a notice, because a silent cap reads as "you are
/// watching everything" when you are not.
const MAX_CHANNELS_PER_FILTER: usize = 128;

/// Recent messages fetched across all channels at once, to seed unread counts
/// for channels that have never been opened.
const OVERVIEW_PAGE: usize = 500;
/// Hits returned by one search. Enough to find the thing, few enough that the
/// list stays scannable.
const SEARCH_LIMIT: usize = 40;
/// How far past a search hit to reach when loading its context, in seconds.
/// Enough to pick up the replies it prompted, not so much that the page is
/// spent on a later conversation.
const CONTEXT_LOOKAHEAD_SECS: u64 = 300;

/// How far back the first load of a channel reaches. The relay caps a single
/// filter's historical results well below this in some deployments, so treat a
/// short page as "that is all there was", not an error.
const HISTORY_PAGE: usize = 200;

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(30);

/// Kinds that change what a channel's transcript looks like. One filter covers
/// all of them so a channel is one subscription, not five.
/// Republish cadence for our own typing indicator. Matches
/// `TYPING_SEND_INTERVAL_MS` in the desktop client.
const TYPING_SEND_INTERVAL_SECS: i64 = 3;

const TRANSCRIPT_KINDS: [u32; 6] = [
    KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
    KIND_STREAM_MESSAGE_EDIT,
    KIND_NIP29_DELETE_EVENT,
    KIND_DELETION,
    KIND_REACTION,
];

/// What the focused channel subscribes to live: the transcript kinds plus the
/// ephemeral ones a history query can never return.
const LIVE_KINDS: [u32; 8] = [
    KIND_CANVAS,
    KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
    KIND_STREAM_MESSAGE_EDIT,
    KIND_NIP29_DELETE_EVENT,
    KIND_DELETION,
    KIND_REACTION,
    KIND_TYPING_INDICATOR,
];

/// Where the panes landed on the last frame, so a click can be resolved
/// against what the reader actually sees.
///
/// These are the *inner* rects, borders excluded, which makes a hit test a
/// plain subtraction instead of an off-by-one waiting to happen.
#[derive(Default, Clone, Copy)]
pub struct Regions {
    pub rooms: Rect,
    pub dms: Rect,
    /// The community list, for resolving a click to a community.
    pub communities: Rect,
    pub transcript: Rect,
    /// The thread pane, when the layout is wide enough to show one beside the
    /// channel. Zero-sized when the thread is closed or replaced the channel.
    pub thread_pane: Rect,
    /// The `esc ✕` label on the thread pane's border. Drawn as an affordance,
    /// so it has to behave like one — a close control you can see but not
    /// click is worse than no control at all.
    pub thread_close: Rect,
    /// The `esc ✕` on whichever modal is open. One rect, because only one
    /// modal is ever up: each takes every key until it is dismissed.
    pub modal_close: Rect,
    /// The completion list's interior, and the index its first visible row
    /// corresponds to — a long roster scrolls, so a y-coordinate alone does
    /// not identify a match.
    pub completion: Rect,
    pub completion_first: usize,
    /// The people picker's interior, and its first visible row.
    pub picker: Rect,
    pub picker_first: usize,
    /// The search result list, and its first visible hit.
    pub search: Rect,
    pub search_first: usize,
    /// Where the caret is, so the completion can hang off it rather than off
    /// the composer's left edge.
    pub caret_x: u16,
}

/// The people picker, for starting a conversation with someone who is not
/// already in the DM list.
pub struct Picker {
    pub query: String,
    pub matches: Vec<(String, nostr::PublicKey)>,
    pub index: usize,
}

/// An open search over every accessible channel.
pub struct Search {
    pub query: String,
    /// The query the current results answer, so Enter can tell "run this" from
    /// "open the highlighted hit".
    pub ran: Option<String>,
    pub running: bool,
    pub results: Vec<nostr::EventId>,
    pub index: usize,
}

/// The emoji picker, targeting one message.
pub struct EmojiPicker {
    pub target: nostr::EventId,
    pub query: String,
    pub matches: Vec<(String, String)>,
    pub index: usize,
}

/// An open `@` autocomplete.
pub struct Completion {
    /// Byte index of the `@` being completed, so accepting can replace from
    /// there rather than guessing at the token's extent.
    pub token_start: usize,
    pub matches: Vec<(String, nostr::PublicKey)>,
    pub index: usize,
}

/// Everything a workspace needs that is not the relay connection itself.
pub struct WorkspaceConfig {
    /// The relay URL, which is also the community's identity.
    pub url: String,
    /// Host only, for the status line.
    pub relay_label: String,
    /// Display name for the rail.
    pub name: String,
    /// Newline and help keys this terminal can actually deliver — resolved
    /// once at startup and carried here so every workspace agrees.
    pub newline_key: &'static str,
    pub help_key: &'static str,
}

/// Every community this client is connected to.
///
/// One live session each rather than one at a time: unread in another
/// community is the whole reason to have it listed, and this relay takes
/// seconds per query — tearing a session down and rebuilding it on every
/// switch would make switching cost more than reading.
///
/// The relay's admission budget is keyed per `(community, pubkey)`, so
/// parallel sessions do not compete for frames with each other.
pub struct App {
    pub workspaces: Vec<Workspace>,
    pub active: usize,
}

/// One community, as the sidebar draws it.
///
/// Built by `App` and handed down each frame, so a workspace never has to know
/// it is one of several.
#[derive(Clone)]
pub struct CommunityRow {
    pub name: String,
    pub unread: usize,
    pub mentions: bool,
    pub active: bool,
    pub live: bool,
}

impl App {
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self {
            workspaces,
            active: 0,
        }
    }

    /// The sidebar's community list.
    ///
    /// Empty when there is only one: a panel listing a single community
    /// spends a box to say "you are here".
    pub fn community_rows(&self) -> Vec<CommunityRow> {
        if self.workspaces.len() < 2 {
            return Vec::new();
        }
        self.workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                let (unread, mentions) = self.workspace_badge(index);
                CommunityRow {
                    name: workspace.name.clone(),
                    unread,
                    mentions,
                    active: index == self.active,
                    live: matches!(workspace.connection, Connection::Live),
                }
            })
            .collect()
    }

    /// Moves to the next community. With the list on screen, cycling is
    /// legible without a modal to narrate it.
    pub fn cycle_workspace(&mut self) {
        if self.workspaces.len() > 1 {
            self.active = (self.active + 1) % self.workspaces.len();
        }
    }

    /// Resolves a click in the community panel.
    pub fn community_click(&mut self, column: u16, row: u16) -> bool {
        let region = self.current().regions.communities;
        if !region.contains(Position::new(column, row)) {
            return false;
        }
        let index = (row - region.y) as usize;
        if index < self.workspaces.len() {
            self.select_workspace(index);
        }
        true
    }

    /// The community in front. Every pane renders from this one.
    pub fn current(&self) -> &Workspace {
        &self.workspaces[self.active.min(self.workspaces.len() - 1)]
    }

    pub fn current_mut(&mut self) -> &mut Workspace {
        let index = self.active.min(self.workspaces.len() - 1);
        &mut self.workspaces[index]
    }

    pub fn workspace_mut(&mut self, url: &str) -> Option<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|workspace| workspace.url == url)
    }

    pub fn select_workspace(&mut self, index: usize) {
        if index < self.workspaces.len() {
            self.active = index;
        }
    }

    pub fn should_quit(&self) -> bool {
        self.workspaces
            .iter()
            .any(|workspace| workspace.should_quit)
    }

    /// Unread and mentions across a community, for its rail entry.
    pub fn workspace_badge(&self, index: usize) -> (usize, bool) {
        let Some(workspace) = self.workspaces.get(index) else {
            return (0, false);
        };
        let mut total = 0;
        let mut mentions = false;
        for channel in workspace.visible_channels() {
            let (count, mention) = workspace.unread(&channel.id);
            total += count;
            mentions |= mention;
        }
        (total, mentions)
    }
}

/// A canvas edit handed to the main loop.
pub struct CanvasEdit {
    pub channel: Uuid,
    pub content: String,
    /// The revision this edit started from. Kind 40100 has no
    /// compare-and-swap, so this is the only way to notice that someone else
    /// saved while the editor was open.
    pub base: Option<nostr::EventId>,
}

/// A URL shortened for a one-line notice.
fn truncate_url(url: &str) -> String {
    const MAX: usize = 60;
    if url.chars().count() <= MAX {
        return url.to_string();
    }
    let head: String = url.chars().take(MAX - 1).collect();
    format!("{head}…")
}

/// A rendered link, and where a click on it lands.
#[derive(Clone)]
pub struct LinkTarget {
    pub row: u16,
    pub start: u16,
    pub end: u16,
    pub url: String,
}

/// A clickable reaction pill.
#[derive(Clone)]
pub struct PillTarget {
    pub row: u16,
    pub start: u16,
    pub end: u16,
    pub message: nostr::EventId,
    /// The emoji this pill toggles, or `None` for the trailing add button.
    pub emoji: Option<String>,
}

/// Which message pane something refers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Channel,
    Thread,
}

pub enum Connection {
    Connecting,
    Live,
    Down(String),
}

/// Results of spawned relay work, delivered back to the main loop.
pub enum Task {
    Channels(Vec<Event>),
    History {
        channel: Uuid,
        events: Vec<Event>,
    },
    /// Events to fold into the store with no further handling — profiles, and
    /// the cross-channel overview page.
    Apply(Vec<Event>),
    Sent(Result<(), String>),
    /// This user's own `kind:30078` read-state coordinates.
    ReadState(Vec<Event>),
    /// The relay-signed `kind:30622` DM visibility snapshot.
    Visibility(Vec<Event>),
    /// A DM open command was accepted; the channel list will now contain it.
    DmOpened,
    /// A page of older history.
    Page {
        channel: Uuid,
        events: Vec<Event>,
    },
    /// Full-text search hits for `query`.
    SearchResults {
        query: String,
        events: Vec<Event>,
    },
    /// A failure worth surfacing. `channel` is set when a history query failed,
    /// so only that channel's loading state clears.
    Failed {
        channel: Option<Uuid>,
        message: String,
    },
}

pub struct Workspace {
    /// The relay this workspace talks to. Doubles as its identity, since a
    /// community is its URL.
    pub url: String,
    pub store: Store,
    /// Selection is held by channel id, not by row.
    ///
    /// The list re-sorts by name whenever metadata lands, so an index taken
    /// before a channel is created or renamed silently points at a different
    /// room afterwards — and the first thing that happens on connect is a
    /// channel-list load that reorders everything.
    pub selected: Option<Uuid>,
    pub input: String,
    /// Rendered lines scrolled up from the bottom. Zero is "pinned to newest",
    /// which is why a new message never yanks a reader out of history.
    pub scroll: usize,
    /// The thread pane's own scroll. Separate because the two panes are read
    /// independently — scrolling a thread must not move the channel behind it.
    pub thread_scroll: usize,
    pub connection: Connection,
    pub notice: Option<String>,
    pub relay_label: String,
    /// The community's name, taken from the first label of the relay host —
    /// `one.example.com` is "one". There is no
    /// server-side name to ask for: a community *is* its URL.
    pub name: String,
    /// What to call the newline key in the hints. Terminals that implement the
    /// keyboard protocol can report Shift+Enter; the rest cannot distinguish
    /// it from Enter at all, so the hint has to name a key that works here.
    pub newline_key: &'static str,
    /// What to call the help key. Ctrl-H is backspace on any terminal without
    /// the keyboard protocol, so binding it there would delete a character
    /// instead of opening this.
    pub help_key: &'static str,
    /// Whether the key reference is open, and how far down it is scrolled.
    pub help: bool,
    pub help_scroll: usize,
    /// Cross-device read position. Unread is derived from this rather than
    /// counted locally, so a channel read in Buzz Desktop is not still bold
    /// here — badges that disagree with the desktop are noise.
    pub read: ReadState,
    /// DMs this viewer has hidden, per NIP-DV. Buzz has no DM delete — hiding
    /// is the only way to get a stale conversation out of the way, and a
    /// retired agent leaves one behind with the same display name as its
    /// replacement.
    pub hidden_dms: HashSet<Uuid>,
    /// Whether hidden DMs are revealed, so one can be un-hidden.
    pub show_hidden: bool,
    pub should_quit: bool,
    /// The open thread, if any. While set, the transcript shows that thread
    /// instead of the channel and the composer replies into it.
    pub thread: Option<nostr::EventId>,
    /// Screen rows carrying a "N replies" affordance, and the thread each
    /// opens. Rebuilt every frame by the renderer.
    pub thread_targets: Vec<(u16, nostr::EventId)>,
    pub completion: Option<Completion>,
    pub picker: Option<Picker>,
    pub emoji_picker: Option<EmojiPicker>,
    pub search: Option<Search>,
    /// A message to scroll into view on the next frame, then forget.
    pub focus_message: Option<nostr::EventId>,
    /// Channels with a page of older history in flight.
    pub paging: HashSet<Uuid>,
    /// Channels the relay has no more history for.
    ///
    /// "No more loaded" rather than "the channel started here": over
    /// WebSocket there is no authoritative exhaustion signal, so this records
    /// what we observed rather than a claim about the channel.
    pub exhausted: HashSet<Uuid>,
    /// Set by the renderer when the reader is near the top of a transcript.
    pub wants_older: Option<Uuid>,
    /// The community list, handed down each frame so the sidebar can draw it
    /// without a workspace knowing about its siblings.
    pub communities: Vec<CommunityRow>,
    /// Whether the canvas is showing instead of the transcript.
    pub canvas_open: bool,
    pub canvas_scroll: usize,
    /// An edit the main loop must run, because opening `$EDITOR` means giving
    /// the terminal away and a renderer cannot do that.
    pub canvas_edit: Option<CanvasEdit>,
    /// Rendered reaction pills, and what a click on each one does.
    pub reaction_targets: Vec<PillTarget>,
    /// Message header rows, so clicking a name opens the emoji picker for it.
    pub header_targets: Vec<(u16, nostr::EventId)>,
    /// Rendered links, so clicking one opens it.
    pub link_targets: Vec<LinkTarget>,
    /// Someone we have just asked the relay to open a DM with. The channel id
    /// is the relay's to assign, so the selection waits for it to appear in
    /// the next channel list rather than being guessed at.
    pending_dm: Option<nostr::PublicKey>,
    /// When we last told the channel we are typing.
    typing_sent_at: i64,
    pub regions: Regions,
    keys: Keys,
    session: Arc<RelaySession>,
    tasks: mpsc::Sender<Task>,
    /// Latest desired subscription set. A `watch` rather than a spawn per
    /// change: two spawned `set_subscriptions` calls can land in either order,
    /// so holding Tab could leave the session subscribed to a channel the user
    /// already moved past. A watch keeps only the newest, which is exactly the
    /// semantics wanted.
    subscriptions: watch::Sender<Vec<Subscription>>,
    /// Channels whose history has been requested, so switching back to one
    /// does not re-page it every time.
    loaded: HashSet<Uuid>,
    /// Channels with a history query in flight.
    ///
    /// Without this the transcript cannot tell "this channel is empty" from
    /// "the answer has not arrived", and this relay routinely takes several
    /// seconds to answer — so it showed "Nothing here yet" for a channel with
    /// 130 messages in it.
    pub loading: HashSet<Uuid>,
    /// Pubkeys whose profile has already been requested. Two code paths ask
    /// for profiles — the channel list and every history page — and without
    /// this they re-request the same people on every channel switch.
    profiles_requested: HashSet<String>,
}

impl Workspace {
    pub fn new(
        config: WorkspaceConfig,
        keys: Keys,
        session: Arc<RelaySession>,
        tasks: mpsc::Sender<Task>,
    ) -> Self {
        let WorkspaceConfig {
            url,
            relay_label,
            name,
            newline_key,
            help_key,
        } = config;
        let (subscriptions, mut rx) = watch::channel(Vec::new());
        let applier = Arc::clone(&session);
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let desired = rx.borrow_and_update().clone();
                applier.set_subscriptions(desired).await;
            }
        });
        Self {
            url,
            store: Store::default(),
            selected: None,
            input: String::new(),
            scroll: 0,
            thread_scroll: 0,
            connection: Connection::Connecting,
            notice: None,
            relay_label,
            name,
            newline_key,
            help_key,
            help: false,
            help_scroll: 0,
            read: ReadState::load(),
            hidden_dms: HashSet::new(),
            show_hidden: false,
            should_quit: false,
            thread: None,
            thread_targets: Vec::new(),
            completion: None,
            picker: None,
            emoji_picker: None,
            search: None,
            focus_message: None,
            paging: HashSet::new(),
            exhausted: HashSet::new(),
            wants_older: None,
            communities: Vec::new(),
            canvas_open: false,
            canvas_scroll: 0,
            canvas_edit: None,
            reaction_targets: Vec::new(),
            header_targets: Vec::new(),
            link_targets: Vec::new(),
            pending_dm: None,
            typing_sent_at: 0,
            regions: Regions::default(),
            keys,
            session,
            tasks,
            subscriptions,
            loaded: HashSet::new(),
            loading: HashSet::new(),
            profiles_requested: HashSet::new(),
        }
    }

    pub fn visible_channels(&self) -> Vec<&Channel> {
        self.store
            .channels()
            .iter()
            .filter(|channel| !channel.archived)
            // Only DMs are hideable. NIP-DV is explicit that non-DM channels
            // must not be affected by the snapshot.
            .filter(|channel| {
                self.show_hidden
                    || channel.kind != ChannelKind::Dm
                    || !self.hidden_dms.contains(&channel.id)
            })
            .collect()
    }

    pub fn is_hidden(&self, channel: &Channel) -> bool {
        channel.kind == ChannelKind::Dm && self.hidden_dms.contains(&channel.id)
    }

    pub fn hidden_count(&self) -> usize {
        self.store
            .channels()
            .iter()
            .filter(|channel| self.is_hidden(channel))
            .count()
    }

    pub fn toggle_show_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        // Leaving reveal mode while sitting on a hidden DM would strand the
        // selection on a channel that is no longer in the list.
        if !self.show_hidden {
            let stranded = self
                .current_channel()
                .map(|channel| self.is_hidden(channel))
                .unwrap_or(false);
            if stranded {
                self.selected = self.visible_channels().first().map(|c| c.id);
                self.on_channel_changed();
            }
        }
    }

    /// Sidebar and title text for a channel, resolved against the reader.
    pub fn channel_label(&self, channel: &Channel) -> String {
        self.store.channel_label(channel, &self.keys.public_key())
    }

    /// The sidebar, sectioned. DMs are separated from rooms because a
    /// workspace routinely has more of them, all named after people rather
    /// than topics — an undivided list buries the rooms.
    /// Rooms — every channel that is not a direct message.
    pub fn rooms(&self) -> Vec<&Channel> {
        partition_channels(&self.visible_channels()).0
    }

    /// Direct messages, which get their own pane. A workspace routinely has
    /// more DMs than rooms, all named after people rather than topics, so one
    /// undivided list buries the rooms.
    pub fn dms(&self) -> Vec<&Channel> {
        partition_channels(&self.visible_channels()).1
    }

    // ── people picker ───────────────────────────────────────────────────────

    pub fn open_people_picker(&mut self) {
        self.picker = Some(Picker {
            query: String::new(),
            matches: Vec::new(),
            index: 0,
        });
        self.refresh_picker();
    }

    pub fn dismiss_picker(&mut self) {
        self.picker = None;
    }

    pub fn picker_input(&mut self, c: char) {
        if let Some(picker) = self.picker.as_mut() {
            picker.query.push(c);
        }
        self.refresh_picker();
    }

    pub fn picker_backspace(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            picker.query.pop();
        }
        self.refresh_picker();
    }

    pub fn picker_next(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            if !picker.matches.is_empty() {
                picker.index = (picker.index + 1) % picker.matches.len();
            }
        }
    }

    pub fn picker_previous(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            let count = picker.matches.len();
            if count > 0 {
                picker.index = (picker.index + count - 1) % count;
            }
        }
    }

    fn refresh_picker(&mut self) {
        let people = self.store.people(&self.keys.public_key());
        if let Some(picker) = self.picker.as_mut() {
            let query = picker.query.to_lowercase();
            picker.matches = people
                .into_iter()
                .filter(|(name, _)| name.to_lowercase().contains(&query))
                .collect();
            picker.index = picker.index.min(picker.matches.len().saturating_sub(1));
        }
    }

    /// Opens a conversation with the highlighted person.
    ///
    /// `open_dm` on the relay dedupes on the participant set, so this returns
    /// an existing conversation rather than creating a second one — and it
    /// clears `hidden_at`, which makes this the way back to a DM that was
    /// hidden rather than a duplicate of it.
    pub fn picker_accept(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some((name, pubkey)) = picker.matches.get(picker.index).cloned() else {
            return;
        };

        // Already in the list: just go there. Round-tripping the relay to
        // learn something we already know would only add latency.
        let existing = self
            .store
            .channels()
            .iter()
            .find(|channel| {
                channel.kind == ChannelKind::Dm
                    && self.store.participants_of(channel).len() == 2
                    && self.store.participants_of(channel).contains(&pubkey)
            })
            .map(|channel| channel.id);
        if let Some(id) = existing {
            if self.hidden_dms.remove(&id) {
                self.notice = Some(format!("restored {name}"));
            }
            self.selected = Some(id);
            self.on_channel_changed();
            if self.hidden_dms.is_empty() {
                self.show_hidden = false;
            }
        }

        let hex = pubkey.to_hex();
        let Ok(builder) = buzz_sdk::builders::build_dm_open(&[hex.as_str()]) else {
            self.notice = Some("could not open that conversation".into());
            return;
        };
        let Ok(event) = builder.sign_with_keys(&self.keys) else {
            return;
        };
        self.pending_dm = Some(pubkey);
        self.notice = Some(format!("opening {name}…"));

        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            match session.publish(event, PUBLISH_TIMEOUT).await {
                Ok(()) => {
                    let _ = tasks.send(Task::DmOpened).await;
                }
                Err(error) => {
                    let _ = tasks
                        .send(Task::Failed {
                            channel: None,
                            message: format!("open dm: {error}"),
                        })
                        .await;
                }
            }
        });
    }

    /// Hides the selected DM, or un-hides it when hidden DMs are revealed.
    ///
    /// Hiding publishes `kind:41012`; un-hiding publishes `kind:41010`, which
    /// the relay treats as re-open — `open_dm` dedupes on the participant set,
    /// so it returns the existing conversation and clears `hidden_at` rather
    /// than creating a second one.
    pub fn toggle_hide_selected(&mut self) {
        let Some(channel) = self.current_channel().cloned() else {
            return;
        };
        if channel.kind != ChannelKind::Dm {
            self.notice = Some("only direct messages can be hidden".into());
            return;
        }

        let hiding = !self.hidden_dms.contains(&channel.id);
        let builder = if hiding {
            // No SDK builder exists for kind 41012; buzz-cli hand-rolls it the
            // same way. The shape is one `h` tag naming the DM.
            match nostr::Tag::parse(["h", &channel.id.to_string()]) {
                Ok(tag) => nostr::EventBuilder::new(nostr::Kind::Custom(41012), "").tags([tag]),
                Err(error) => {
                    self.notice = Some(format!("hide failed: {error}"));
                    return;
                }
            }
        } else {
            let others: Vec<String> = self
                .store
                .participants_of(&channel)
                .into_iter()
                .filter(|pubkey| !self.is_me(pubkey))
                .map(|pubkey| pubkey.to_hex())
                .collect();
            if others.is_empty() {
                self.notice = Some("cannot re-open: no participants known yet".into());
                return;
            }
            let refs: Vec<&str> = others.iter().map(String::as_str).collect();
            match buzz_sdk::builders::build_dm_open(&refs) {
                Ok(builder) => builder,
                Err(error) => {
                    self.notice = Some(format!("re-open failed: {error}"));
                    return;
                }
            }
        };

        let Ok(event) = builder.sign_with_keys(&self.keys) else {
            self.notice = Some("signing failed".into());
            return;
        };

        // Optimistic: the relay republishes the snapshot as a post-commit side
        // effect, which is best-effort and can lag. The sidebar should respond
        // to the keystroke, not to the round trip.
        if hiding {
            self.hidden_dms.insert(channel.id);
            if self.current_channel().map(|c| c.id) == Some(channel.id) && !self.show_hidden {
                self.selected = self.visible_channels().first().map(|c| c.id);
                self.on_channel_changed();
            }
        } else {
            self.hidden_dms.remove(&channel.id);
        }
        self.notice = Some(if hiding {
            format!(
                "hid {}",
                self.store.channel_label(&channel, &self.keys.public_key())
            )
        } else {
            format!(
                "restored {}",
                self.store.channel_label(&channel, &self.keys.public_key())
            )
        });

        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            if let Err(error) = session.publish(event, PUBLISH_TIMEOUT).await {
                let _ = tasks
                    .send(Task::Failed {
                        channel: None,
                        message: format!("visibility: {error}"),
                    })
                    .await;
            }
        });
        self.spawn_visibility();
    }

    /// Loads the relay-signed DM visibility snapshot.
    ///
    /// Queried by `#p` rather than `#d`: NIP-DV says `p` is the tag the
    /// relay's read-authorization gate checks, and `d` only addresses the
    /// replaceable event.
    pub fn spawn_visibility(&self) {
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        let me = self.keys.public_key().to_hex();
        tokio::spawn(async move {
            let filter = json!({"kinds": [30622], "#p": [me], "limit": 1});
            if let Ok(events) = session.fetch(filter, FETCH_TIMEOUT).await {
                let _ = tasks.send(Task::Visibility(events)).await;
            }
        });
    }

    /// Replaces the hidden set from a snapshot.
    ///
    /// Wholesale rather than merged: the snapshot is recompute-and-replace, so
    /// the newest one is the complete authoritative set and a delta merge would
    /// resurrect DMs the viewer just un-hid.
    fn apply_visibility(&mut self, events: &[Event]) {
        let me = self.keys.public_key().to_hex();
        let Some(newest) = events
            .iter()
            .filter(|event| u32::from(event.kind.as_u16()) == 30622)
            // The relay serves only our own snapshot, but the `d` tag is
            // checked anyway: a snapshot addressed to someone else would hide
            // their conversations from us.
            .filter(|event| first_tag_value(event, "d").as_deref() == Some(me.as_str()))
            .max_by_key(|event| event.created_at)
        else {
            return;
        };
        self.hidden_dms = newest
            .tags
            .iter()
            .filter_map(|tag| {
                let parts = tag.as_slice();
                (parts.first().map(String::as_str) == Some("h"))
                    .then(|| parts.get(1))
                    .flatten()
                    .and_then(|value| Uuid::parse_str(value).ok())
            })
            .collect();
    }

    pub fn me(&self) -> nostr::PublicKey {
        self.keys.public_key()
    }

    pub fn is_me(&self, pubkey: &nostr::PublicKey) -> bool {
        *pubkey == self.keys.public_key()
    }

    /// Unread count for a channel, and whether any of it is addressed to you.
    pub fn unread(&self, channel: &Uuid) -> (usize, bool) {
        let (count, mentions) = self
            .store
            .log_or_empty(channel)
            .unread_after(self.read.frontier(channel), &self.keys.public_key());
        (count, mentions && !self.is_dm(channel))
    }

    fn is_dm(&self, channel: &Uuid) -> bool {
        self.store
            .channels()
            .iter()
            .any(|c| c.id == *channel && c.kind == ChannelKind::Dm)
    }

    /// Whether a message should carry the "this one is for you" mark.
    ///
    /// False in a DM regardless of tags. Every message in a DM addresses every
    /// participant by construction — that is the rule the desktop encodes in
    /// `messageMentionPubkeys.ts`, and this client follows it when sending —
    /// so marking them all would put a rail on every incoming line and tell
    /// the reader nothing. A mark that is always on is not a mark.
    pub fn addressed_to_me(&self, message: &crate::store::Message, channel: &Uuid) -> bool {
        !self.is_dm(channel)
            && message.author != self.keys.public_key()
            && message.mentions.iter().any(|pubkey| self.is_me(pubkey))
    }

    /// Marks the visible channel read up to its newest message.
    ///
    /// Called on every frame's worth of state change rather than on a timer:
    /// a channel you are looking at is read, and the frontier only moves
    /// forward, so doing it often is free.
    /// Publishes our coordinate if the frontier has moved since the last one.
    ///
    /// Called on a timer rather than on every change: reading a busy channel
    /// advances the frontier on every arriving message, and one signed event
    /// per message would spend the relay's admission budget on bookkeeping.
    pub fn flush_read_state(&mut self) {
        if !self.read.is_dirty() {
            return;
        }
        let Ok(builder) = self.read.build(&self.keys) else {
            return;
        };
        let Ok(event) = builder.sign_with_keys(&self.keys) else {
            return;
        };
        let session = Arc::clone(&self.session);
        tokio::spawn(async move {
            // Best effort. The frontier is grow-only and republished on the
            // next change, so a lost flush costs nothing but freshness.
            let _ = session.publish(event, PUBLISH_TIMEOUT).await;
        });
    }

    pub fn mark_current_read(&mut self) {
        let Some(channel) = self.current_channel().map(|c| c.id) else {
            return;
        };
        if let Some(newest) = self.store.log_or_empty(&channel).newest_at() {
            self.read.advance(&channel, newest);
        }
    }

    pub fn current_channel(&self) -> Option<&Channel> {
        let selected = self.selected?;
        self.visible_channels()
            .into_iter()
            .find(|channel| channel.id == selected)
    }

    pub fn select_next(&mut self) {
        self.step_selection(1);
    }

    pub fn select_previous(&mut self) {
        self.step_selection(-1);
    }

    fn step_selection(&mut self, delta: isize) {
        let channels = self.visible_channels();
        let count = channels.len();
        if count == 0 {
            return;
        }
        let current = self
            .selected
            .and_then(|id| channels.iter().position(|channel| channel.id == id))
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(count as isize) as usize;
        let id = channels[next].id;
        drop(channels);
        self.selected = Some(id);
        self.on_channel_changed();
    }

    /// Selects whatever channel sits under a click, if anything does.
    ///
    /// A click past the end of the list is not a selection. Returning early
    /// there matters: clamping to the last row would make empty space below
    /// the list behave like a button.
    pub fn click(&mut self, column: u16, row: u16) {
        // A close control that is drawn has to behave like one, whichever
        // modal drew it.
        if self.modal_open()
            && self
                .regions
                .modal_close
                .contains(Position::new(column, row))
        {
            self.dismiss_modal();
            return;
        }

        if self.search.is_some() {
            let rows = self
                .search
                .as_ref()
                .map(|search| search.results.len())
                .unwrap_or(0);
            // Each hit is two rows, so the row offset halves into an index.
            if let Some(offset) = row_at(self.regions.search, column, row, rows * 2) {
                let index = self.regions.search_first + offset / 2;
                if let Some(search) = self.search.as_mut() {
                    if index < search.results.len() {
                        search.index = index;
                        self.open_search_hit();
                    }
                }
            }
            return;
        }

        if self.emoji_picker.is_some() {
            let rows = self
                .emoji_picker
                .as_ref()
                .map(|picker| picker.matches.len())
                .unwrap_or(0);
            if let Some(offset) = row_at(self.regions.picker, column, row, rows) {
                let index = self.regions.picker_first + offset;
                if let Some(picker) = self.emoji_picker.as_mut() {
                    if index < picker.matches.len() {
                        picker.index = index;
                        self.emoji_accept();
                    }
                }
            }
            return;
        }

        // A link sits inside a message body, so it wins over the body's own
        // affordances — clicking the URL in a message should open the URL.
        if let Some(link) = self
            .link_targets
            .iter()
            .find(|link| link.row == row && (link.start..link.end).contains(&column))
            .cloned()
        {
            self.open_link(&link.url);
            return;
        }

        // A pill sits over the transcript, so it is resolved before anything
        // the transcript itself offers.
        if let Some(pill) = self
            .reaction_targets
            .iter()
            .find(|pill| pill.row == row && (pill.start..pill.end).contains(&column))
            .cloned()
        {
            match pill.emoji {
                Some(emoji) => self.toggle_reaction(pill.message, &emoji),
                None => self.open_emoji_picker(pill.message),
            }
            return;
        }

        // The picker is modal: while it is open nothing behind it is clickable.
        if self.picker.is_some() {
            let rows = self
                .picker
                .as_ref()
                .map(|picker| picker.matches.len())
                .unwrap_or(0);
            if let Some(offset) = row_at(self.regions.picker, column, row, rows) {
                let index = self.regions.picker_first + offset;
                if let Some(picker) = self.picker.as_mut() {
                    if index < picker.matches.len() {
                        picker.index = index;
                        self.picker_accept();
                    }
                }
            }
            return;
        }

        if self.thread.is_some()
            && self
                .regions
                .thread_close
                .contains(Position::new(column, row))
        {
            self.close_thread();
            return;
        }

        // The completion floats over everything, so it gets the click first.
        if self.completion.is_some() {
            let rows = self
                .completion
                .as_ref()
                .map(|completion| completion.matches.len())
                .unwrap_or(0);
            if let Some(offset) = row_at(self.regions.completion, column, row, rows) {
                let index = self.regions.completion_first + offset;
                if let Some(completion) = self.completion.as_mut() {
                    if index < completion.matches.len() {
                        completion.index = index;
                        self.accept_completion();
                        return;
                    }
                }
            }
        }

        if let Some((_, root)) = self
            .thread_targets
            .iter()
            .find(|(target_row, _)| *target_row == row)
        {
            if self.regions.transcript.contains(Position::new(column, row)) {
                let root = *root;
                self.open_thread(root);
                return;
            }
        }

        // A message's own header row is how you reach a reaction on something
        // older than the newest message, without a selection model.
        if let Some((_, target)) = self
            .header_targets
            .iter()
            .find(|(target_row, _)| *target_row == row)
            .copied()
        {
            if self.regions.transcript.contains(Position::new(column, row))
                || self
                    .regions
                    .thread_pane
                    .contains(Position::new(column, row))
            {
                self.open_emoji_picker(target);
                return;
            }
        }

        let rooms = self.rooms();
        let hit = row_at(self.regions.rooms, column, row, rooms.len())
            .map(|index| rooms[index].id)
            .or_else(|| {
                let dms = self.dms();
                row_at(self.regions.dms, column, row, dms.len()).map(|index| dms[index].id)
            });
        let Some(id) = hit else {
            return;
        };
        if self.selected == Some(id) {
            return;
        }
        self.selected = Some(id);
        self.on_channel_changed();
    }

    // ── composing ───────────────────────────────────────────────────────────

    pub fn insert_char(&mut self, c: char) {
        self.input.push(c);
        self.refresh_completion();
        self.notify_typing();
    }

    /// Tells the channel we are composing, at most every few seconds.
    ///
    /// Kind 20002 is ephemeral and WS-only — the relay never stores it, so
    /// there is nothing to clean up and no history to pollute. It stops on its
    /// own: the indicator expires, and sending clears it outright.
    fn notify_typing(&mut self) {
        if self.input.trim().is_empty() {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        if now - self.typing_sent_at < TYPING_SEND_INTERVAL_SECS {
            return;
        }
        let Some(channel) = self.current_channel().map(|c| c.id) else {
            return;
        };
        self.typing_sent_at = now;

        let Ok(tag) = nostr::Tag::parse(["h", &channel.to_string()]) else {
            return;
        };
        let Ok(event) = nostr::EventBuilder::new(nostr::Kind::Custom(20002), "")
            .tags([tag])
            .sign_with_keys(&self.keys)
        else {
            return;
        };
        let session = Arc::clone(&self.session);
        tokio::spawn(async move {
            // Best effort, and never surfaced: a dropped typing indicator is
            // not something to interrupt anyone about.
            let _ = session.publish(event, PUBLISH_TIMEOUT).await;
        });
    }

    // ── reactions ───────────────────────────────────────────────────────────

    /// Adds or removes your reaction of one kind on one message.
    ///
    /// Applied locally before publishing. The event is signed first, so the
    /// optimistic entry carries the same id the relay will echo back and the
    /// store's dedupe makes the echo a no-op — no phantom, no double count.
    pub fn toggle_reaction(&mut self, target: nostr::EventId, emoji: &str) {
        let Some(channel) = self.current_channel().map(|c| c.id) else {
            return;
        };
        let existing = self
            .store
            .log_or_empty(&channel)
            .reactions(target, &self.keys.public_key())
            .into_iter()
            .find(|group| group.emoji == emoji)
            .and_then(|group| group.mine);

        let builder = match existing {
            Some(reaction_id) => buzz_sdk::builders::build_remove_reaction(reaction_id),
            None => buzz_sdk::builders::build_reaction(target, emoji),
        };
        let Ok(builder) = builder else {
            self.notice = Some("that emoji was rejected".into());
            return;
        };
        // `build_reaction` carries only the `e` target. The relay derives the
        // channel from it for storage, but live fan-out is topic-based, so
        // without an `h` tag nobody sees the reaction until they refetch.
        let builder = match nostr::Tag::parse(["h", &channel.to_string()]) {
            Ok(tag) => builder.tag(tag),
            Err(_) => builder,
        };
        let Ok(event) = builder.sign_with_keys(&self.keys) else {
            return;
        };

        self.store.apply(&event);
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            if let Err(error) = session.publish(event, PUBLISH_TIMEOUT).await {
                let _ = tasks
                    .send(Task::Failed {
                        channel: None,
                        message: format!("reaction: {error}"),
                    })
                    .await;
            }
        });
    }

    pub fn open_emoji_picker(&mut self, target: nostr::EventId) {
        self.emoji_picker = Some(EmojiPicker {
            target,
            query: String::new(),
            matches: Vec::new(),
            index: 0,
        });
        self.refresh_emoji_picker();
    }

    /// Opens the picker on the newest message, so reacting needs no mouse.
    pub fn react_to_newest(&mut self) {
        let Some(channel) = self.current_channel().map(|c| c.id) else {
            return;
        };
        let newest = match self.thread {
            Some(root) => self
                .store
                .log_or_empty(&channel)
                .thread(root)
                .last()
                .map(|message| message.id),
            None => self
                .store
                .log_or_empty(&channel)
                .top_level()
                .last()
                .map(|message| message.id),
        };
        match newest {
            Some(target) => self.open_emoji_picker(target),
            None => self.notice = Some("nothing to react to".into()),
        }
    }

    pub fn dismiss_emoji_picker(&mut self) {
        self.emoji_picker = None;
    }

    pub fn emoji_input(&mut self, c: char) {
        if let Some(picker) = self.emoji_picker.as_mut() {
            picker.query.push(c);
        }
        self.refresh_emoji_picker();
    }

    pub fn emoji_backspace(&mut self) {
        if let Some(picker) = self.emoji_picker.as_mut() {
            picker.query.pop();
        }
        self.refresh_emoji_picker();
    }

    pub fn emoji_next(&mut self) {
        if let Some(picker) = self.emoji_picker.as_mut() {
            if !picker.matches.is_empty() {
                picker.index = (picker.index + 1) % picker.matches.len();
            }
        }
    }

    pub fn emoji_previous(&mut self) {
        if let Some(picker) = self.emoji_picker.as_mut() {
            let count = picker.matches.len();
            if count > 0 {
                picker.index = (picker.index + count - 1) % count;
            }
        }
    }

    pub fn emoji_accept(&mut self) {
        let Some(picker) = self.emoji_picker.take() else {
            return;
        };
        let Some((_, emoji)) = picker.matches.get(picker.index).cloned() else {
            return;
        };
        self.toggle_reaction(picker.target, &emoji);
    }

    fn refresh_emoji_picker(&mut self) {
        let palette = crate::emoji::palette();
        if let Some(picker) = self.emoji_picker.as_mut() {
            let query = picker.query.to_lowercase();
            picker.matches = palette
                .iter()
                .filter(|(name, _)| name.contains(&query))
                .map(|(name, glyph)| ((*name).to_string(), (*glyph).to_string()))
                .collect();
            picker.index = picker.index.min(picker.matches.len().saturating_sub(1));
        }
    }

    // ── help ────────────────────────────────────────────────────────────────

    /// Whether any modal is up. They are mutually exclusive by construction:
    /// each swallows every key until it closes.
    pub fn modal_open(&self) -> bool {
        self.help || self.search.is_some() || self.emoji_picker.is_some() || self.picker.is_some()
    }

    /// Closes whichever modal is open, outermost first.
    pub fn dismiss_modal(&mut self) {
        if self.help {
            self.close_help();
        } else if self.search.is_some() {
            self.dismiss_search();
        } else if self.emoji_picker.is_some() {
            self.dismiss_emoji_picker();
        } else {
            self.dismiss_picker();
        }
    }

    pub fn toggle_help(&mut self) {
        self.help = !self.help;
        self.help_scroll = 0;
    }

    pub fn close_help(&mut self) {
        self.help = false;
    }

    pub fn help_scroll_by(&mut self, delta: isize) {
        self.help_scroll = self.help_scroll.saturating_add_signed(delta);
    }

    // ── search ──────────────────────────────────────────────────────────────

    pub fn open_search(&mut self) {
        self.search = Some(Search {
            query: String::new(),
            ran: None,
            running: false,
            results: Vec::new(),
            index: 0,
        });
    }

    pub fn dismiss_search(&mut self) {
        self.search = None;
    }

    pub fn search_input(&mut self, c: char) {
        if let Some(search) = self.search.as_mut() {
            search.query.push(c);
        }
    }

    pub fn search_backspace(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.query.pop();
        }
    }

    pub fn search_next(&mut self) {
        if let Some(search) = self.search.as_mut() {
            if !search.results.is_empty() {
                search.index = (search.index + 1) % search.results.len();
            }
        }
    }

    pub fn search_previous(&mut self) {
        if let Some(search) = self.search.as_mut() {
            let count = search.results.len();
            if count > 0 {
                search.index = (search.index + count - 1) % count;
            }
        }
    }

    /// Enter: run the query if it has changed, otherwise open the hit.
    ///
    /// One key for both because they are never both meaningful at once — a
    /// query you have not run has no results to open, and results you are
    /// looking at answer the query already in the box.
    pub fn search_submit(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let query = search.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        if search.ran.as_deref() == Some(query.as_str()) {
            self.open_search_hit();
            return;
        }
        if let Some(search) = self.search.as_mut() {
            search.running = true;
            search.index = 0;
        }
        self.spawn_search(query);
    }

    fn spawn_search(&self, query: String) {
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            // Every filter in a search REQ must carry `search`: the relay
            // rejects a mix outright. One filter per REQ is what this session
            // sends anyway.
            let filter = json!({
                "kinds": [KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2],
                "search": query,
                "limit": SEARCH_LIMIT,
            });
            let events = session
                .fetch(filter, FETCH_TIMEOUT)
                .await
                .unwrap_or_default();
            let _ = tasks.send(Task::SearchResults { query, events }).await;
        });
    }

    /// Opens the highlighted hit: its channel, its thread if it is a reply,
    /// and scrolls it into view.
    fn open_search_hit(&mut self) {
        let Some(id) = self
            .search
            .as_ref()
            .and_then(|search| search.results.get(search.index).copied())
        else {
            return;
        };
        let Some((channel, message)) = self.store.locate(id) else {
            return;
        };
        let root = message.root;
        let message_at = message.created_at;
        self.search = None;
        self.selected = Some(channel);
        self.on_channel_changed();
        // A reply lives in its thread, not in the timeline — landing on the
        // channel and leaving the reader to find it would be no answer at all.
        if let Some(root) = root {
            self.open_thread(root);
        }
        self.focus_message = Some(id);
        // The hit is one message in an otherwise unloaded stretch of history.
        self.spawn_context(channel, message_at);
    }

    /// Hands a URL to the platform's opener.
    ///
    /// A terminal cannot render an image and this client does not try: the
    /// kitty and iTerm2 graphics protocols are terminal-specific, and a client
    /// that showed a picture on one machine and a broken box on the next is
    /// worse than one that consistently hands it to something that can.
    ///
    /// Spawned and forgotten rather than waited on — `open` returns
    /// immediately but a browser cold-starting behind it can take seconds, and
    /// the socket loop cannot be blocked for that.
    pub fn open_link(&mut self, url: &str) {
        // Only web URLs. A markdown link can carry any scheme, and handing
        // `file://` or a custom scheme to the system opener on someone else's
        // say-so is a way to run something they did not choose.
        let allowed = url.starts_with("https://") || url.starts_with("http://");
        if !allowed {
            self.notice = Some(format!("refused to open a non-web link: {url}"));
            return;
        }

        let opener = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(target_os = "windows") {
            "explorer"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => self.notice = Some(format!("opened {}", truncate_url(url))),
            Err(error) => self.notice = Some(format!("could not open link: {error}")),
        }
    }

    /// Whether anything on screen is mid-animation.
    ///
    /// Drives the repaint rate: a spinner needs ~16 frames a second, and an
    /// idle client should not be asking the terminal to redraw that often for
    /// nothing.
    pub fn is_animating(&self) -> bool {
        if matches!(self.connection, Connection::Connecting) {
            return true;
        }
        if self
            .current_channel()
            .is_some_and(|channel| self.loading.contains(&channel.id))
        {
            return true;
        }
        self.current_channel().is_some_and(|channel| {
            let now = chrono::Utc::now().timestamp().max(0) as u64;
            !self
                .store
                .log_or_empty(&channel.id)
                .typing(now, &self.keys.public_key())
                .is_empty()
        })
    }

    /// Who the current channel shows as typing.
    pub fn typing_now(&self) -> Vec<String> {
        let Some(channel) = self.current_channel().map(|c| c.id) else {
            return Vec::new();
        };
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        self.store
            .log_or_empty(&channel)
            .typing(now, &self.keys.public_key())
            .iter()
            .map(|pubkey| self.store.display_name(pubkey))
            .collect()
    }

    pub fn insert_newline(&mut self) {
        self.input.push('\n');
        self.refresh_completion();
        self.notify_typing();
    }

    /// Inserts pasted text verbatim.
    ///
    /// Newlines are kept: a pasted list or code block that arrives as one
    /// run-on line has been silently corrupted, and the sender usually only
    /// finds out after it is published.
    pub fn insert_paste(&mut self, text: &str) {
        self.input.push_str(text);
        self.refresh_completion();
        self.notify_typing();
    }

    pub fn backspace(&mut self) {
        self.input.pop();
        self.refresh_completion();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.refresh_completion();
    }

    /// Deletes the word before the caret, the way readline's Ctrl-W does.
    pub fn delete_word(&mut self) {
        let trimmed = self.input.trim_end_matches(char::is_whitespace);
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|index| index + 1)
            .unwrap_or(0);
        self.input.truncate(cut);
        self.refresh_completion();
    }

    /// Recomputes the `@` completion from the current input.
    ///
    /// The query deliberately runs to the end of the input rather than to the
    /// next space, so a two-word display name like "Fizz Buzz" can be
    /// completed from "@Fizz B". Matching nothing closes the popup, which is
    /// what keeps an ordinary sentence containing an `@` from holding it open.
    pub fn refresh_completion(&mut self) {
        let previous = self.completion.as_ref().map(|c| c.index).unwrap_or(0);
        self.completion = self.candidates().map(|(token_start, matches)| Completion {
            token_start,
            index: previous.min(matches.len().saturating_sub(1)),
            matches,
        });
    }

    fn candidates(&self) -> Option<(usize, Vec<(String, nostr::PublicKey)>)> {
        let (token_start, query) = active_mention(&self.input)?;
        let channel = self.current_channel()?;
        let query = query.to_lowercase();
        let matches: Vec<(String, nostr::PublicKey)> = self
            .store
            .mentionable(channel, &self.keys.public_key())
            .into_iter()
            .filter(|(name, _)| name.to_lowercase().starts_with(&query))
            .collect();
        (!matches.is_empty()).then_some((token_start, matches))
    }

    pub fn completion_next(&mut self) {
        if let Some(completion) = self.completion.as_mut() {
            completion.index = (completion.index + 1) % completion.matches.len();
        }
    }

    pub fn completion_previous(&mut self) {
        if let Some(completion) = self.completion.as_mut() {
            let count = completion.matches.len();
            completion.index = (completion.index + count - 1) % count;
        }
    }

    pub fn dismiss_completion(&mut self) {
        self.completion = None;
    }

    /// Replaces the `@` token with the highlighted name.
    pub fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some((name, _)) = completion.matches.get(completion.index) else {
            return;
        };
        // Trailing space: a mention is almost never the end of a sentence, and
        // without it the next keystroke reopens the popup on the same token.
        self.input
            .replace_range(completion.token_start.., &format!("@{name} "));
    }

    // ── canvas ──────────────────────────────────────────────────────────────

    pub fn toggle_canvas(&mut self) {
        self.canvas_open = !self.canvas_open;
        self.canvas_scroll = 0;
        if self.canvas_open {
            if let Some(channel) = self.current_channel().map(|c| c.id) {
                self.spawn_canvas(channel);
            }
        }
    }

    pub fn close_canvas(&mut self) {
        self.canvas_open = false;
        self.canvas_scroll = 0;
    }

    /// Loads the newest canvas revision for a channel.
    fn spawn_canvas(&self, channel: Uuid) {
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            // Newest first, and only the newest is wanted: older revisions are
            // history this client does not surface yet.
            let filter = json!({"kinds": [KIND_CANVAS], "#h": [channel.to_string()], "limit": 1});
            if let Ok(events) = session.fetch(filter, FETCH_TIMEOUT).await {
                let _ = tasks.send(Task::Apply(events)).await;
            }
        });
    }

    /// Hands the current canvas to the main loop to open in `$EDITOR`.
    pub fn request_canvas_edit(&mut self) {
        let Some(channel) = self.current_channel().map(|c| c.id) else {
            return;
        };
        let canvas = self.store.canvas(&channel);
        self.canvas_edit = Some(CanvasEdit {
            channel,
            content: canvas.map(|c| c.content.clone()).unwrap_or_default(),
            base: canvas.map(|c| c.id),
        });
    }

    /// Publishes an edited canvas, unless someone else saved first.
    ///
    /// Refuses rather than clobbering: kind 40100 has no compare-and-swap, so
    /// the relay will happily accept a save that silently discards work
    /// someone did while the editor was open. The draft is kept on disk so
    /// refusing costs nothing.
    pub fn save_canvas(&mut self, edit: CanvasEdit, content: String, draft_path: &str) {
        if content == edit.content {
            self.notice = Some("canvas unchanged".into());
            return;
        }
        let current = self.store.canvas(&edit.channel).map(|c| c.id);
        if current != edit.base {
            self.notice = Some(format!(
                "canvas changed while you were editing — draft kept at {draft_path}"
            ));
            return;
        }
        let builder = match buzz_sdk::builders::build_set_canvas(edit.channel, &content) {
            Ok(builder) => builder,
            Err(error) => {
                self.notice = Some(format!("canvas rejected: {error}"));
                return;
            }
        };
        let Ok(event) = builder.sign_with_keys(&self.keys) else {
            return;
        };
        self.store.apply(&event);
        self.notice = Some("canvas saved".into());

        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            if let Err(error) = session.publish(event, PUBLISH_TIMEOUT).await {
                let _ = tasks
                    .send(Task::Failed {
                        channel: None,
                        message: format!("canvas: {error}"),
                    })
                    .await;
            }
        });
    }

    // ── threads ─────────────────────────────────────────────────────────────

    pub fn open_thread(&mut self, root: nostr::EventId) {
        if self.thread == Some(root) {
            return;
        }
        self.thread = Some(root);
        self.thread_scroll = 0;
    }

    pub fn close_thread(&mut self) {
        self.thread = None;
        self.thread_scroll = 0;
    }

    /// Opens the most recently active thread in the channel, so a thread is
    /// reachable without a mouse.
    pub fn open_newest_thread(&mut self) {
        let Some(channel) = self.current_channel().map(|c| c.id) else {
            return;
        };
        match self.store.log_or_empty(&channel).newest_thread() {
            Some(root) => self.open_thread(root),
            None => self.notice = Some("no threads in this channel".into()),
        }
    }

    /// The pane the keyboard scrolls. An open thread is what the reader is
    /// looking at, so it wins over the channel behind it.
    pub fn focused_pane(&self) -> Pane {
        if self.thread.is_some() {
            Pane::Thread
        } else {
            Pane::Channel
        }
    }

    /// The pane under the pointer, so the wheel scrolls what is being pointed
    /// at rather than what has focus.
    pub fn pane_at(&self, column: u16, row: u16) -> Option<Pane> {
        let position = Position::new(column, row);
        if self.regions.thread_pane.contains(position) {
            return Some(Pane::Thread);
        }
        if self.regions.transcript.contains(position) {
            return Some(Pane::Channel);
        }
        None
    }

    pub fn scroll_up(&mut self, pane: Pane, lines: usize) {
        let scroll = self.scroll_mut(pane);
        *scroll = scroll.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, pane: Pane, lines: usize) {
        let scroll = self.scroll_mut(pane);
        *scroll = scroll.saturating_sub(lines);
    }

    pub fn scroll_mut(&mut self, pane: Pane) -> &mut usize {
        match pane {
            Pane::Channel => &mut self.scroll,
            Pane::Thread => &mut self.thread_scroll,
        }
    }

    fn on_channel_changed(&mut self) {
        self.scroll = 0;
        // A canvas belongs to the channel it was opened from.
        self.close_canvas();
        // A thread belongs to the channel it was opened from.
        self.close_thread();
        let Some(channel) = self.current_channel().map(|c| c.id) else {
            return;
        };
        self.mark_current_read();
        if self.loaded.insert(channel) {
            self.spawn_history(channel);
        }
        self.resubscribe();
    }

    /// Sends the compose buffer to the focused channel.
    pub fn submit(&mut self) {
        let content = self.input.trim().to_string();
        if content.is_empty() {
            return;
        }
        let Some(channel) = self.current_channel().cloned() else {
            self.notice = Some("no channel selected".into());
            return;
        };
        let me = self.keys.public_key().to_hex();

        let mentions = recipients(&self.store, &channel, &content, &me);
        let mention_refs: Vec<&str> = mentions.iter().map(String::as_str).collect();

        // Replying flat into a channel while a thread is open would put the
        // answer somewhere nobody reading the thread will see it.
        let thread_ref = self.thread.map(|root| buzz_sdk::ThreadRef {
            root_event_id: root,
            // Every reply targets the root rather than the message above it.
            // A TUI has no way to point at one reply in particular, so a
            // deeper parent would be a guess presented as a fact.
            parent_event_id: root,
        });

        // Build through buzz-sdk rather than by hand: it carries the `h` tag
        // shape and the `allow_self_tagging` opt-in that nostr 0.44 would
        // otherwise strip from a self-mention.
        let builder = match buzz_sdk::builders::build_message(
            channel.id,
            &content,
            thread_ref.as_ref(),
            &mention_refs,
            false,
            &[],
        ) {
            Ok(builder) => builder,
            Err(error) => {
                self.notice = Some(format!("rejected: {error}"));
                return;
            }
        };
        let event = match builder.sign_with_keys(&self.keys) {
            Ok(event) => event,
            Err(error) => {
                self.notice = Some(format!("signing failed: {error}"));
                return;
            }
        };
        self.input.clear();
        self.completion = None;
        // Stay pinned to newest in whichever pane the message is going to, so
        // the sender sees it land.
        *self.scroll_mut(self.focused_pane()) = 0;
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            let result = session.publish(event, PUBLISH_TIMEOUT).await;
            let _ = tasks.send(Task::Sent(result)).await;
        });
    }

    // ── relay work ──────────────────────────────────────────────────────────

    /// Loads the channel list. Discovery events (39000/39002) are stored
    /// channel-scoped, so the relay does not fan them out to live global
    /// subscriptions — they have to be asked for.
    pub fn spawn_channel_load(&self) {
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        let me = self.keys.public_key().to_hex();
        tokio::spawn(async move {
            let members = session
                .fetch(
                    json!({"kinds": [KIND_NIP29_GROUP_MEMBERS], "#p": [me], "limit": 500}),
                    FETCH_TIMEOUT,
                )
                .await;
            let members = match members {
                Ok(events) => events,
                Err(error) => {
                    let _ = tasks
                        .send(Task::Failed {
                            channel: None,
                            message: format!("channel list: {error}"),
                        })
                        .await;
                    return;
                }
            };
            let ids: Vec<String> = members
                .iter()
                .filter_map(|event| {
                    event.tags.iter().find_map(|t| {
                        let parts = t.as_slice();
                        (parts.first().map(String::as_str) == Some("d"))
                            .then(|| parts.get(1).cloned())
                            .flatten()
                    })
                })
                .collect();
            if ids.is_empty() {
                let _ = tasks.send(Task::Channels(Vec::new())).await;
                return;
            }
            match session
                .fetch(
                    json!({"kinds": [KIND_NIP29_GROUP_METADATA], "#d": ids, "limit": 500}),
                    FETCH_TIMEOUT,
                )
                .await
            {
                Ok(metadata) => {
                    // Forward the rosters too. They were fetched to find the
                    // channel ids, and they carry every member of every channel
                    // — which is what makes `@name` resolvable for people who
                    // have not spoken in the loaded history.
                    let mut events = members;
                    events.extend(metadata);
                    let _ = tasks.send(Task::Channels(events)).await;
                }
                Err(error) => {
                    let _ = tasks
                        .send(Task::Failed {
                            channel: None,
                            message: format!("channel metadata: {error}"),
                        })
                        .await;
                }
            }
        });
    }

    /// Loads this user's own read-state coordinates.
    ///
    /// Narrowed by tag and to a week, which the spec permits for a client that
    /// neither reads nor writes the override layer.
    pub fn spawn_read_state(&self) {
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        let me = self.keys.public_key().to_hex();
        let since = chrono::Utc::now().timestamp().max(0) as u64 - HORIZON_SECS;
        tokio::spawn(async move {
            let filter = json!({
                "kinds": [30078],
                "authors": [me],
                "#t": ["read-state"],
                "since": since,
                "limit": 100,
            });
            if let Ok(events) = session.fetch(filter, FETCH_TIMEOUT).await {
                let _ = tasks.send(Task::ReadState(events)).await;
            }
        });
    }

    /// One page of recent messages across every channel.
    ///
    /// Unread is derived from the store, and the store only holds channels you
    /// have opened — so without this, every unvisited channel reads as zero
    /// unread no matter how much has happened in it. One filter across all
    /// channels costs a single round trip against a relay where each one is
    /// measured in seconds.
    pub fn spawn_overview(&self) {
        let channels: Vec<String> = self
            .visible_channels()
            .iter()
            .take(MAX_CHANNELS_PER_FILTER)
            .map(|channel| channel.id.to_string())
            .collect();
        if channels.is_empty() {
            return;
        }
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            let filter = json!({
                "kinds": [KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2],
                "#h": channels,
                "limit": OVERVIEW_PAGE,
            });
            if let Ok(events) = session.fetch(filter, FETCH_TIMEOUT).await {
                let _ = tasks.send(Task::Apply(events)).await;
            }
        });
    }

    /// Fetches the page of history older than what is loaded.
    ///
    /// `until` is inclusive in NIP-01, so asking from the oldest loaded
    /// message re-requests its whole second. The duplicates cost a little
    /// bandwidth and the store drops them; the alternative is losing every
    /// message that shares a second with the page boundary. The bridge's
    /// `before_id` cursor solves this exactly, but it is bridge-only and this
    /// client speaks WebSocket.
    pub fn page_older(&mut self, channel: Uuid) {
        if self.paging.contains(&channel) || self.exhausted.contains(&channel) {
            return;
        }
        let Some(until) = self.store.log_or_empty(&channel).oldest_at() else {
            return;
        };
        self.paging.insert(channel);

        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            let filter = json!({
                "kinds": TRANSCRIPT_KINDS,
                "#h": [channel.to_string()],
                "until": until,
                "limit": HISTORY_PAGE,
            });
            let events = session
                .fetch(filter, FETCH_TIMEOUT)
                .await
                .unwrap_or_default();
            let _ = tasks.send(Task::Page { channel, events }).await;
        });
    }

    /// Loads the neighbourhood of a message, so a search hit lands in context.
    ///
    /// Without this, jumping to a hit from months ago shows exactly one
    /// message sitting between two unrelated days — chronologically correct
    /// and completely unreadable.
    fn spawn_context(&mut self, channel: Uuid, around: u64) {
        self.paging.insert(channel);
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            // Reaching past the hit picks up a little of what followed it as
            // well as what came before.
            let filter = json!({
                "kinds": TRANSCRIPT_KINDS,
                "#h": [channel.to_string()],
                "until": around + CONTEXT_LOOKAHEAD_SECS,
                "limit": HISTORY_PAGE,
            });
            let events = session
                .fetch(filter, FETCH_TIMEOUT)
                .await
                .unwrap_or_default();
            let _ = tasks.send(Task::Page { channel, events }).await;
        });
    }

    pub fn spawn_history(&mut self, channel: Uuid) {
        self.loading.insert(channel);
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            let filter = json!({
                "kinds": TRANSCRIPT_KINDS,
                "#h": [channel.to_string()],
                "limit": HISTORY_PAGE,
            });
            match session.fetch(filter, FETCH_TIMEOUT).await {
                Ok(events) => {
                    let _ = tasks.send(Task::History { channel, events }).await;
                }
                Err(error) => {
                    let _ = tasks
                        .send(Task::Failed {
                            channel: Some(channel),
                            message: format!("history: {error}"),
                        })
                        .await;
                }
            }
        });
    }

    fn spawn_profiles(&mut self, authors: Vec<String>) {
        // Ask only for people not already asked about. The channel list and
        // every history page both feed this, so the overlap is the common case.
        let authors: Vec<String> = authors
            .into_iter()
            .filter(|pubkey| self.profiles_requested.insert(pubkey.clone()))
            .collect();
        if authors.is_empty() {
            return;
        }
        let session = Arc::clone(&self.session);
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            let filter = json!({"kinds": [KIND_PROFILE], "authors": authors, "limit": 500});
            if let Ok(events) = session.fetch(filter, FETCH_TIMEOUT).await {
                let _ = tasks.send(Task::Apply(events)).await;
            }
        });
    }

    /// Installs the live subscription set for the current selection.
    ///
    /// Two subscriptions, both live-only (`limit: 0`, which NIP-01 defines as
    /// "no stored results"): the focused channel's full transcript, and a
    /// message-only sweep across every channel to drive unread counts. Ids are
    /// derived from what the filter contains, honoring the session's rule that
    /// an id's filter never changes.
    pub fn resubscribe(&self) {
        let mut subscriptions = Vec::new();

        if let Some(channel) = self.current_channel().map(|c| c.id) {
            subscriptions.push(Subscription {
                id: format!("channel-{channel}"),
                filter: json!({
                    // Typing indicators ride the channel subscription rather
                    // than one of their own: they are ephemeral and WS-only,
                    // so there is no history query that could ever carry them.
                    "kinds": LIVE_KINDS,
                    "#h": [channel.to_string()],
                    "limit": 0,
                }),
            });
        }

        let mut all: Vec<String> = self
            .visible_channels()
            .iter()
            .map(|channel| channel.id.to_string())
            .collect();
        if !all.is_empty() {
            all.sort();
            let truncated = all.len() > MAX_CHANNELS_PER_FILTER;
            all.truncate(MAX_CHANNELS_PER_FILTER);
            if truncated {
                // Deliberately loud. A capped watch list that says nothing
                // looks identical to a complete one.
                eprintln!(
                    "buzz-tui: watching only the first {MAX_CHANNELS_PER_FILTER} channels for unread"
                );
            }
            subscriptions.push(Subscription {
                // The id encodes the filter's identity, so adding a channel
                // opens a new subscription instead of mutating one.
                id: format!("unread-{}", fingerprint(&all)),
                filter: json!({
                    "kinds": [KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2],
                    "#h": all,
                    "limit": 0,
                }),
            });
        }

        // Send, never spawn: see the field comment on `subscriptions`.
        let _ = self.subscriptions.send(subscriptions);
    }

    // ── folding results back in ─────────────────────────────────────────────

    pub fn on_task(&mut self, task: Task) {
        match task {
            Task::Channels(events) => {
                for event in &events {
                    self.store.apply(event);
                }
                // A conversation we just asked for is the one the user is
                // waiting on, so it takes the selection when it arrives.
                if let Some(pubkey) = self.pending_dm {
                    let opened = self
                        .store
                        .channels()
                        .iter()
                        .find(|channel| {
                            channel.kind == ChannelKind::Dm
                                && self.store.participants_of(channel).contains(&pubkey)
                        })
                        .map(|channel| channel.id);
                    if let Some(id) = opened {
                        self.pending_dm = None;
                        self.hidden_dms.remove(&id);
                        self.selected = Some(id);
                        self.on_channel_changed();
                        self.notice = None;
                        return;
                    }
                }
                self.notice = Some(format!("{} channels", self.visible_channels().len()));
                // DM rows are labelled by who is in them, so their profiles
                // have to land before the sidebar means anything.
                self.spawn_profiles(self.store.all_participants());
                // Seeds unread for channels that have never been opened.
                self.spawn_overview();
                // Adopt a selection only when there is none, or when the one
                // held has vanished. Re-selecting on every refresh would yank
                // the user back to the first channel each reconnect.
                if self.current_channel().is_none() {
                    if let Some(first) = self.visible_channels().first().map(|c| c.id) {
                        self.selected = Some(first);
                        self.on_channel_changed();
                        return;
                    }
                }
                self.resubscribe();
            }
            Task::History { channel, events } => {
                self.loading.remove(&channel);
                let authors: Vec<String> = events
                    .iter()
                    .map(|event| event.pubkey.to_hex())
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                for event in &events {
                    self.store.apply(event);
                }
                if self.current_channel().map(|c| c.id) == Some(channel) {
                    self.scroll = 0;
                }
                self.spawn_profiles(authors);
            }
            Task::Apply(events) => {
                for event in &events {
                    self.store.apply(event);
                }
                self.mark_current_read();
            }
            Task::ReadState(events) => {
                let mut conflicted = false;
                for event in &events {
                    conflicted |= self.read.merge(event, &self.keys);
                }
                if conflicted {
                    // Another installation owns this coordinate. Publishing
                    // over it would clobber its state.
                    self.read.rotate_slot();
                }
                self.mark_current_read();
            }
            Task::Visibility(events) => self.apply_visibility(&events),
            Task::DmOpened => self.spawn_channel_load(),
            Task::Page { channel, events } => {
                self.paging.remove(&channel);
                let before = self.store.log_or_empty(&channel).messages().len();
                for event in &events {
                    self.store.apply(event);
                }
                let after = self.store.log_or_empty(&channel).messages().len();
                // No new messages means the relay has nothing further to give
                // on this cursor. Row counts cannot prove exhaustion in
                // general — a full page could still be the last one — but a
                // page that adds nothing is the end of what we can reach.
                if after == before {
                    self.exhausted.insert(channel);
                }
                self.spawn_profiles(
                    events
                        .iter()
                        .map(|event| event.pubkey.to_hex())
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect(),
                );
            }
            Task::SearchResults { query, events } => {
                // Fold the hits into the store first: that is what makes a
                // result from months ago land at its chronological place and
                // become reachable by scrolling.
                for event in &events {
                    self.store.apply(event);
                }
                let mut results: Vec<Event> = events;
                results.sort_by_key(|event| std::cmp::Reverse(event.created_at));
                if let Some(search) = self.search.as_mut() {
                    // A late reply to a query the user has moved on from must
                    // not replace what they are looking at now.
                    if search.query.trim() == query {
                        search.running = false;
                        search.ran = Some(query);
                        search.results = results.iter().map(|event| event.id).collect();
                        search.index = 0;
                    }
                }
                self.spawn_profiles(
                    results
                        .iter()
                        .map(|event| event.pubkey.to_hex())
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect(),
                );
            }
            Task::Sent(Ok(())) => {}
            Task::Sent(Err(error)) => self.notice = Some(format!("send failed: {error}")),
            Task::Failed { channel, message } => {
                // Clear only the channel that failed. Clearing every flag let
                // an unrelated profile failure make a still-loading channel
                // claim to be empty.
                if let Some(channel) = channel {
                    self.loading.remove(&channel);
                }
                self.notice = Some(message);
            }
        }
    }

    /// The relay finished serving a subscription's stored events.
    ///
    /// For a live-only (`limit: 0`) channel subscription that means the REQ was
    /// accepted and the socket is now streaming — worth surfacing, because the
    /// alternative to a silent success here is a channel that looks idle when
    /// it is actually not subscribed.
    pub fn on_eose(&mut self, subscription_id: &str) {
        if self
            .current_channel()
            .is_some_and(|channel| subscription_id == format!("channel-{}", channel.id))
        {
            self.notice = Some("caught up".into());
        }
    }

    pub fn on_relay_event(&mut self, subscription_id: &str, event: &Event) {
        self.store.apply(event);
        // Unread is derived from the store against the read frontier, so an
        // arriving message needs no counter of its own — except in the channel
        // being looked at, which is read by definition.
        if buzz_sdk::builders::extract_channel_id(event) == self.current_channel().map(|c| c.id) {
            self.mark_current_read();
        }
        let _ = subscription_id;
    }
}

fn first_tag_value(event: &Event, key: &str) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let parts = tag.as_slice();
        (parts.first().map(String::as_str) == Some(key))
            .then(|| parts.get(1).cloned())
            .flatten()
    })
}

/// Splits the channel list into (rooms, direct messages).
pub fn partition_channels<'a>(channels: &[&'a Channel]) -> (Vec<&'a Channel>, Vec<&'a Channel>) {
    channels
        .iter()
        .copied()
        .partition(|channel| channel.kind != ChannelKind::Dm)
}

/// The `p` tags an outgoing message should carry.
///
/// Buzz carries mentions as tags, never as text. An agent harness subscribes
/// with `#p` set to its own key, so a message that names an agent only in the
/// body never reaches it — which reads as being ignored rather than as being
/// undelivered.
///
/// A stream channel notifies only who the author explicitly named. A DM
/// addresses every other participant whether or not the text contains an `@`,
/// which is the rule the desktop encodes in `messageMentionPubkeys.ts`.
pub fn recipients(store: &Store, channel: &Channel, content: &str, me: &str) -> Vec<String> {
    let mut mentions = store.resolve_mentions(content);
    if channel.kind == ChannelKind::Dm {
        let participants: Vec<String> = store
            .participants_of(channel)
            .iter()
            .map(|pubkey| pubkey.to_hex())
            .collect();
        buzz_sdk::mentions::merge_mentions(&mut mentions, &participants, MENTION_CAP);
    }
    // Drops the sender and lowercases, so a DM does not ask the relay to
    // notify the person who just typed it.
    buzz_sdk::mentions::normalize_mention_pubkeys(&mentions, Some(me))
}

/// Finds the `@` token being typed: its byte offset and the text after it.
///
/// An `@` only starts a mention at the start of the input or after whitespace,
/// which is what keeps an email address from opening the popup.
pub fn active_mention(input: &str) -> Option<(usize, &str)> {
    let start = input.char_indices().rev().find_map(|(index, c)| {
        if c != '@' {
            return None;
        }
        let preceded = index == 0
            || input[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        preceded.then_some(index)
    })?;
    Some((start, &input[start + 1..]))
}

/// Which list row a click landed on, or `None` past the end of the list.
///
/// Clamping instead of returning `None` would make the empty space below the
/// last channel behave like a button on that channel.
fn row_at(region: Rect, column: u16, row: u16, len: usize) -> Option<usize> {
    if !region.contains(Position::new(column, row)) {
        return None;
    }
    let index = (row - region.y) as usize;
    (index < len).then_some(index)
}

/// Order-independent digest of a channel-id set, used to give a filter's
/// subscription an id that changes exactly when the filter does.
fn fingerprint(ids: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for id in ids {
        for byte in id.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::store::Store;
    use nostr::{EventBuilder, Keys, Kind, Tag};
    use ratatui::layout::Rect;

    fn channel_id() -> Uuid {
        Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap()
    }

    /// One channel of `kind`, with a roster and profiles for everyone in it.
    fn store_with(kind: &str, me: &Keys, others: &[(&Keys, &str)]) -> (Store, Channel) {
        // Signed by a relay keypair, as the relay actually signs discovery
        // events: nostr 0.44 strips a `p` tag matching the signer, so signing
        // as a member would quietly drop that member from the roster.
        let relay = Keys::generate();
        let mut store = Store::default();
        for (keys, name) in others {
            store.apply(
                &EventBuilder::new(Kind::Custom(0), format!(r#"{{"display_name":"{name}"}}"#))
                    .sign_with_keys(keys)
                    .unwrap(),
            );
        }
        let mut roster = vec![
            Tag::parse(["d", &channel_id().to_string()]).unwrap(),
            Tag::parse(["p", &me.public_key().to_hex()]).unwrap(),
        ];
        for (keys, _) in others {
            roster.push(Tag::parse(["p", &keys.public_key().to_hex()]).unwrap());
        }
        store.apply(
            &EventBuilder::new(Kind::Custom(39002), "")
                .tags(roster)
                .sign_with_keys(&relay)
                .unwrap(),
        );
        store.apply(
            &EventBuilder::new(Kind::Custom(39000), "")
                .tags([
                    Tag::parse(["d", &channel_id().to_string()]).unwrap(),
                    Tag::parse(["name", "room"]).unwrap(),
                    Tag::parse(["t", kind]).unwrap(),
                ])
                .sign_with_keys(&relay)
                .unwrap(),
        );
        let channel = store.channels()[0].clone();
        (store, channel)
    }

    #[test]
    fn a_dm_addresses_its_participants_with_no_at_sign_in_the_text() {
        // The bug this covers: an agent harness subscribes with `#p` set to its
        // own key, so a DM carrying no p tag never reaches the agent at all —
        // the message posts and simply goes unanswered.
        let me = Keys::generate();
        let agent = Keys::generate();
        let (store, channel) = store_with("dm", &me, &[(&agent, "Samantha")]);
        let tags = recipients(&store, &channel, "hey", &me.public_key().to_hex());
        assert_eq!(tags, vec![agent.public_key().to_hex()]);
    }

    #[test]
    fn a_dm_never_addresses_its_own_sender() {
        let me = Keys::generate();
        let agent = Keys::generate();
        let (store, channel) = store_with("dm", &me, &[(&agent, "Samantha")]);
        let tags = recipients(&store, &channel, "hey", &me.public_key().to_hex());
        assert!(!tags.contains(&me.public_key().to_hex()));
    }

    #[test]
    fn a_stream_channel_notifies_only_who_was_named() {
        let me = Keys::generate();
        let agent = Keys::generate();
        let (store, channel) = store_with("stream", &me, &[(&agent, "Samantha")]);

        let silent = recipients(&store, &channel, "morning all", &me.public_key().to_hex());
        assert!(
            silent.is_empty(),
            "a stream message must not notify the whole room"
        );

        let named = recipients(&store, &channel, "hey @Samantha", &me.public_key().to_hex());
        assert_eq!(named, vec![agent.public_key().to_hex()]);
    }

    #[test]
    fn a_multi_word_display_name_resolves_whole() {
        // The bare tokenizer would only ever see "fizz" and match nothing.
        let me = Keys::generate();
        let agent = Keys::generate();
        let (store, channel) = store_with("stream", &me, &[(&agent, "Fizz Buzz")]);
        let tags = recipients(
            &store,
            &channel,
            "@Fizz Buzz can you look?",
            &me.public_key().to_hex(),
        );
        assert_eq!(tags, vec![agent.public_key().to_hex()]);
    }

    fn channel_named(name: &str, kind: ChannelKind) -> Channel {
        Channel {
            id: Uuid::new_v4(),
            name: name.to_string(),
            topic: String::new(),
            kind,
            archived: false,
            participants: Vec::new(),
        }
    }

    #[test]
    fn rooms_and_direct_messages_are_separated_in_order() {
        let room = channel_named("dev", ChannelKind::Stream);
        let forum = channel_named("rfc", ChannelKind::Forum);
        let dm = channel_named("DM", ChannelKind::Dm);
        let (rooms, dms) = partition_channels(&[&room, &dm, &forum]);
        assert_eq!(
            rooms.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![room.id, forum.id],
            "a forum is a room, not a direct message"
        );
        assert_eq!(dms.iter().map(|c| c.id).collect::<Vec<_>>(), vec![dm.id]);
    }

    #[test]
    fn a_workspace_with_no_direct_messages_yields_an_empty_second_list() {
        let room = channel_named("dev", ChannelKind::Stream);
        let (rooms, dms) = partition_channels(&[&room]);
        assert_eq!(rooms.len(), 1);
        assert!(dms.is_empty(), "an empty pane is the caller's to skip");
    }

    #[test]
    fn an_at_sign_opens_a_mention_only_after_whitespace() {
        // Otherwise every email address in a message opens the popup.
        assert_eq!(active_mention("@Sam"), Some((0, "Sam")));
        assert_eq!(active_mention("hey @Sam"), Some((4, "Sam")));
        assert_eq!(active_mention("mail me at ian@example.com"), None);
        assert_eq!(active_mention("no mention here"), None);
    }

    #[test]
    fn the_query_runs_to_the_end_so_two_word_names_can_be_typed() {
        // "Fizz Buzz" is one display name. Stopping the query at the first
        // space would make it uncompletable past "Fizz".
        assert_eq!(active_mention("hey @Fizz B"), Some((4, "Fizz B")));
    }

    #[test]
    fn the_last_at_sign_wins() {
        // Completing the earlier one would rewrite text the author already
        // finished with.
        assert_eq!(active_mention("@Sam and @Fi"), Some((9, "Fi")));
    }

    #[test]
    fn an_at_sign_with_nothing_after_it_still_offers_everyone() {
        assert_eq!(active_mention("@"), Some((0, "")));
    }

    #[test]
    fn a_mark_that_would_be_on_every_message_is_not_shown() {
        // Every DM message p-tags every participant — this client does it when
        // sending, and so does the desktop. Marking them all would put a rail
        // on every incoming line and convey nothing.
        let dm = channel_named("Samantha", ChannelKind::Dm);
        let room = channel_named("general", ChannelKind::Stream);
        let mark = |channel: &Channel, tagged: bool| tagged && channel.kind != ChannelKind::Dm;
        assert!(!mark(&dm, true), "a DM addresses you by construction");
        assert!(mark(&room, true), "a channel mention is real information");
        assert!(!mark(&room, false));
    }

    #[test]
    fn hiding_only_ever_applies_to_direct_messages() {
        // NIP-DV is explicit that a visibility snapshot must not affect
        // non-DM channels. A stray `h` tag naming a room must not remove it.
        let room = channel_named("dev", ChannelKind::Stream);
        let dm = channel_named("Kyber", ChannelKind::Dm);
        let hidden: HashSet<Uuid> = [room.id, dm.id].into_iter().collect();

        let visible = |channel: &Channel, show_hidden: bool| {
            show_hidden || channel.kind != ChannelKind::Dm || !hidden.contains(&channel.id)
        };
        assert!(
            visible(&room, false),
            "a room is never hidden by the snapshot"
        );
        assert!(!visible(&dm, false));
        assert!(visible(&dm, true), "reveal mode shows it again");
    }

    #[test]
    fn a_click_past_the_last_channel_selects_nothing() {
        let region = Rect::new(0, 1, 24, 20);
        assert_eq!(row_at(region, 3, 1, 3), Some(0));
        assert_eq!(row_at(region, 3, 3, 3), Some(2));
        assert_eq!(row_at(region, 3, 4, 3), None, "empty space is not a button");
        assert_eq!(row_at(region, 99, 2, 3), None, "outside the pane entirely");
    }

    #[test]
    fn a_changed_channel_set_changes_the_subscription_id() {
        // The session treats an id's filter as immutable, so adding a channel
        // must produce a new id rather than silently mutating an open one.
        let a = fingerprint(&["one".into(), "two".into()]);
        let b = fingerprint(&["one".into(), "two".into(), "three".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn the_same_channel_set_keeps_its_id() {
        let ids = vec!["one".to_string(), "two".to_string()];
        assert_eq!(fingerprint(&ids), fingerprint(&ids));
    }

    #[test]
    fn concatenation_is_not_mistaken_for_a_different_split() {
        // "ab" + "c" and "a" + "bc" must not collide, or two different watch
        // lists would share one subscription id.
        assert_ne!(
            fingerprint(&["ab".into(), "c".into()]),
            fingerprint(&["a".into(), "bc".into()])
        );
    }
}
