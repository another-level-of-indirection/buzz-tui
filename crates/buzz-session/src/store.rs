//! Client-side state derived from the relay's event stream.
//!
//! Everything here is a pure fold over events: `apply` takes one event and
//! mutates state, and nothing in this module touches a socket or a terminal.
//! That is deliberate — it is the part most likely to be wrong, and it is
//! testable against recorded JSON with neither of the other two present.
//!
//! Ordering is `(created_at, id)`, never `created_at` alone. Events sharing a
//! second are common in a busy channel, and a timestamp-only sort makes their
//! order depend on arrival, so a backfilled page and a live delivery disagree
//! about the same two messages.

use std::collections::{HashMap, HashSet};

use nostr::{Event, EventId, PublicKey};
use uuid::Uuid;

use buzz_core::kind::{
    KIND_CANVAS, KIND_DELETION, KIND_NIP29_DELETE_EVENT, KIND_NIP29_GROUP_MEMBERS,
    KIND_NIP29_GROUP_METADATA, KIND_PROFILE, KIND_REACTION, KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_EDIT, KIND_STREAM_MESSAGE_V2, KIND_TYPING_INDICATOR,
};
use buzz_sdk::mentions::{extract_at_mentions_with_known, match_names_to_profiles, MentionProfile};

/// How long a typing indicator stands before it is assumed stale.
///
/// There is no "stopped typing" event, so this is the only thing that ever
/// takes one down. Matches `TYPING_INDICATOR_TTL_MS` in the desktop client so
/// the two agree about who is typing.
pub const TYPING_TTL_SECS: u64 = 8;
/// A person is not "typing" for this long after one of their messages lands.
///
/// Their own indicator routinely arrives just after the message it preceded;
/// without this the sender flickers back to typing the instant they stop.
const TYPING_SUPPRESS_SECS: u64 = 2;

/// Kinds the message log folds. Anything else `apply` ignores, which is the
/// whole point of kind dispatch: an unknown kind is not an error.
pub const CHANNEL_MESSAGE_KINDS: [u32; 2] = [KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelKind {
    Stream,
    Forum,
    Dm,
    Other,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub id: Uuid,
    pub name: String,
    pub topic: String,
    pub kind: ChannelKind,
    pub archived: bool,
    /// Participant pubkeys. The relay includes these in kind:39000 for DMs
    /// specifically so a client can label the conversation without a second
    /// round trip for the member roster.
    pub participants: Vec<PublicKey>,
}

/// A channel's shared document.
///
/// Kind 40100 is *not* replaceable, so every save is a new event and the relay
/// keeps them all. The newest wins, which also means the newest silently
/// clobbers: there is no compare-and-swap, so two people saving a minute apart
/// lose one of the edits. Holding the revision id is what lets this client
/// notice that before it publishes over someone.
#[derive(Clone, Debug)]
pub struct Canvas {
    pub id: EventId,
    pub author: PublicKey,
    pub updated_at: u64,
    pub content: String,
}

/// One person's reaction to one message.
#[derive(Clone, Debug)]
pub struct Reaction {
    /// The reaction event itself. Removing a reaction is a NIP-09 deletion
    /// targeting this id, so it has to be kept rather than derived.
    pub id: EventId,
    pub author: PublicKey,
    /// The rendered glyph, or `:shortcode:` for a custom emoji.
    pub emoji: String,
}

/// Reactions of one kind on one message, collapsed for display.
#[derive(Clone, Debug)]
pub struct ReactionGroup {
    pub emoji: String,
    pub count: usize,
    /// Your own reaction of this kind, if you left one — the id a click needs
    /// in order to take it back.
    pub mine: Option<EventId>,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: EventId,
    pub author: PublicKey,
    pub created_at: u64,
    pub content: String,
    pub edited: bool,
    /// Pubkeys this message `p`-tags. Buzz carries mentions as tags, so this
    /// is what "an agent answered you" actually looks like on the wire.
    pub mentions: Vec<PublicKey>,
    /// The thread this message replies into, when it is a reply.
    ///
    /// `None` means top-level. The relay's own contract is that "replies never
    /// enter the channel timeline", so this is what decides whether a message
    /// appears in the channel or only inside its thread.
    pub root: Option<EventId>,
}

/// One channel's messages, plus the side tables needed to fold late-arriving
/// edits and deletions onto them.
#[derive(Default)]
pub struct ChannelLog {
    items: Vec<Message>,
    seen: HashSet<EventId>,
    /// Edits are ordinary events with their own ids and can arrive before the
    /// message they target — during backfill, routinely. Keeping them keyed by
    /// target means the fold does not depend on arrival order.
    pending_edits: HashMap<EventId, (u64, String)>,
    /// Same argument for tombstones.
    deleted: HashSet<EventId>,
    /// Who is currently typing, and when they last said so.
    typing: HashMap<PublicKey, u64>,
    /// Reactions keyed by the message they target. Kept separately from
    /// messages because a reaction routinely arrives before its target does —
    /// during backfill, and whenever someone reacts to something older than
    /// the loaded window.
    reactions: HashMap<EventId, Vec<Reaction>>,
    /// Reaction event ids already folded, so a redelivery is not a second
    /// reaction.
    seen_reactions: HashSet<EventId>,
}

impl ChannelLog {
    pub fn messages(&self) -> &[Message] {
        &self.items
    }

    /// The channel timeline: messages that are not replies.
    pub fn top_level(&self) -> impl Iterator<Item = &Message> {
        self.messages()
            .iter()
            .filter(|message| message.root.is_none())
    }

    /// A thread, root first, then its replies in order.
    ///
    /// The root may be absent — it can predate the loaded window while its
    /// replies do not — so the thread renders from whatever is held rather
    /// than refusing to open.
    pub fn thread(&self, root: EventId) -> Vec<&Message> {
        self.messages()
            .iter()
            .filter(|message| message.id == root || message.root == Some(root))
            .collect()
    }

    /// Reply counts per root, computed in one pass.
    ///
    /// Per-root counting during rendering would be quadratic in a busy
    /// channel; this is called once per frame.
    pub fn reply_counts(&self) -> HashMap<EventId, usize> {
        let mut counts = HashMap::new();
        for message in self.messages() {
            if let Some(root) = message.root {
                *counts.entry(root).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Messages after `frontier` that someone else wrote, and whether any of
    /// them names `me`.
    ///
    /// Counted rather than tracked incrementally so the answer survives a
    /// restart and matches whatever another device has already read.
    pub fn unread_after(&self, frontier: u64, me: &PublicKey) -> (usize, bool) {
        let mut count = 0;
        let mut mentions = false;
        for message in self.messages() {
            if message.created_at <= frontier || message.author == *me {
                continue;
            }
            count += 1;
            mentions |= message.mentions.contains(me);
        }
        (count, mentions)
    }

    /// Newest message timestamp, which is what reading a channel advances the
    /// frontier to.
    pub fn newest_at(&self) -> Option<u64> {
        self.messages().last().map(|message| message.created_at)
    }

    /// Oldest message timestamp — the cursor for paging further back.
    pub fn oldest_at(&self) -> Option<u64> {
        self.messages().first().map(|message| message.created_at)
    }

    /// Reactions on a message, collapsed by emoji in first-seen order.
    ///
    /// First-seen rather than sorted by count: a row that reorders itself as
    /// people react is hard to click and harder to read.
    pub fn reactions(&self, target: EventId, me: &PublicKey) -> Vec<ReactionGroup> {
        let mut groups: Vec<ReactionGroup> = Vec::new();
        for reaction in self.reactions.get(&target).into_iter().flatten() {
            match groups
                .iter_mut()
                .find(|group| group.emoji == reaction.emoji)
            {
                Some(group) => {
                    group.count += 1;
                    if reaction.author == *me {
                        group.mine = Some(reaction.id);
                    }
                }
                None => groups.push(ReactionGroup {
                    emoji: reaction.emoji.clone(),
                    count: 1,
                    mine: (reaction.author == *me).then_some(reaction.id),
                }),
            }
        }
        groups
    }

    fn add_reaction(&mut self, target: EventId, reaction: Reaction) {
        if !self.seen_reactions.insert(reaction.id) {
            return;
        }
        let existing = self.reactions.entry(target).or_default();
        // One reaction of each kind per person. The relay dedupes at ingest,
        // but a client that trusted that would double-count its own optimistic
        // entry against the echo that follows it.
        if existing
            .iter()
            .any(|other| other.author == reaction.author && other.emoji == reaction.emoji)
        {
            return;
        }
        existing.push(reaction);
    }

    fn remove_reaction(&mut self, id: EventId) {
        self.seen_reactions.remove(&id);
        for reactions in self.reactions.values_mut() {
            reactions.retain(|reaction| reaction.id != id);
        }
    }

    /// Who is typing right now, newest first, excluding the reader.
    pub fn typing(&self, now: u64, me: &PublicKey) -> Vec<PublicKey> {
        let mut live: Vec<(&PublicKey, &u64)> = self
            .typing
            .iter()
            .filter(|(pubkey, at)| *pubkey != me && now.saturating_sub(**at) < TYPING_TTL_SECS)
            .collect();
        live.sort_by(|(_, a), (_, b)| b.cmp(a));
        live.into_iter().map(|(pubkey, _)| *pubkey).collect()
    }

    fn note_typing(&mut self, author: PublicKey, at: u64) {
        // An indicator that predates the author's newest message is stale by
        // construction — they typed, then sent.
        let settled = self
            .items
            .iter()
            .rev()
            .find(|message| message.author == author)
            .is_some_and(|message| at < message.created_at + TYPING_SUPPRESS_SECS);
        if settled {
            return;
        }
        self.typing.insert(author, at);
    }

    /// The most recently active thread, for opening one without a mouse.
    pub fn newest_thread(&self) -> Option<EventId> {
        self.items.iter().rev().find_map(|message| message.root)
    }

    fn insert(&mut self, mut message: Message) {
        // Sending is the end of typing. Waiting for the indicator to expire
        // would leave someone "typing" for eight seconds after their message
        // is already on screen.
        self.typing.remove(&message.author);

        if self.deleted.contains(&message.id) {
            return;
        }
        if !self.seen.insert(message.id) {
            return;
        }
        if let Some((_, content)) = self.pending_edits.get(&message.id) {
            message.content = content.clone();
            message.edited = true;
        }
        let key = (message.created_at, message.id.to_hex());
        let at = self
            .items
            .partition_point(|existing| (existing.created_at, existing.id.to_hex()) < key);
        self.items.insert(at, message);
    }

    fn apply_edit(&mut self, target: EventId, created_at: u64, content: String) {
        // Last edit wins, decided by the edit's own timestamp rather than by
        // arrival, so a backfilled older edit cannot clobber a newer one.
        let supersedes = self
            .pending_edits
            .get(&target)
            .is_none_or(|(seen_at, _)| created_at >= *seen_at);
        if !supersedes {
            return;
        }
        self.pending_edits
            .insert(target, (created_at, content.clone()));
        if let Some(message) = self.items.iter_mut().find(|m| m.id == target) {
            message.content = content;
            message.edited = true;
        }
    }

    fn apply_delete(&mut self, target: EventId) {
        self.deleted.insert(target);
        self.items.retain(|m| m.id != target);
    }
}

/// A cached kind-0 profile.
///
/// The raw JSON is kept alongside the extracted name because
/// `match_names_to_profiles` reads the kind-0 body itself — handing it a
/// pre-parsed name would mean reimplementing its precedence rules here.
struct Profile {
    name: String,
    content_json: String,
}

#[derive(Default)]
pub struct Store {
    channels: Vec<Channel>,
    logs: HashMap<Uuid, ChannelLog>,
    profiles: HashMap<PublicKey, Profile>,
    /// Rosters from kind:39002, which the channel-list query already returns
    /// in full — every member of every channel the reader belongs to.
    members: HashMap<Uuid, Vec<PublicKey>>,
    /// Newest canvas revision seen per channel.
    canvases: HashMap<Uuid, Canvas>,
}

impl Store {
    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    pub fn log_or_empty(&self, channel: &Uuid) -> &ChannelLog {
        static EMPTY: std::sync::OnceLock<ChannelLog> = std::sync::OnceLock::new();
        self.logs
            .get(channel)
            .unwrap_or_else(|| EMPTY.get_or_init(ChannelLog::default))
    }

    /// Display name for a pubkey, falling back to a short hex prefix.
    ///
    /// The fallback is 8 characters rather than a full key because the sender
    /// column has to stay narrow, and rather than 4 because Buzz workspaces
    /// routinely hold agent keys generated in the same session.
    pub fn display_name(&self, pubkey: &PublicKey) -> String {
        self.profiles
            .get(pubkey)
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| pubkey.to_hex()[..8].to_string())
    }

    /// Everyone in a channel, from either source the relay offers.
    ///
    /// kind:39002 carries the authoritative roster; kind:39000 additionally
    /// repeats participants for DMs. Neither alone is complete for every
    /// channel, so both feed this.
    pub fn participants_of(&self, channel: &Channel) -> Vec<PublicKey> {
        let mut out: Vec<PublicKey> = self.members.get(&channel.id).cloned().unwrap_or_default();
        for pubkey in &channel.participants {
            if !out.contains(pubkey) {
                out.push(*pubkey);
            }
        }
        out
    }

    /// Members of `channel` who can actually be mentioned, as (name, pubkey).
    ///
    /// Only members with a real kind-0 profile name are offered. Mention
    /// resolution matches names against profiles, so completing to a hex
    /// fallback would insert an `@` that silently fails to tag anyone — an
    /// autocomplete entry that does not work is worse than none.
    pub fn mentionable(&self, channel: &Channel, me: &PublicKey) -> Vec<(String, PublicKey)> {
        let mut out: Vec<(String, PublicKey)> = self
            .participants_of(channel)
            .into_iter()
            .filter(|pubkey| pubkey != me)
            .filter_map(|pubkey| {
                self.profiles
                    .get(&pubkey)
                    .map(|profile| (profile.name.clone(), pubkey))
            })
            .collect();
        out.sort_by_key(|(name, _)| name.to_lowercase());
        out.dedup_by(|(a, _), (b, _)| a == b);
        out
    }

    /// Finds a message anywhere in the loaded state, with the channel it is in.
    ///
    /// Search results are folded into the store like any other event, so a hit
    /// from months ago inserts at its chronological place and jumping to it is
    /// just a scroll rather than a separate fetch.
    pub fn locate(&self, id: EventId) -> Option<(Uuid, &Message)> {
        self.logs.iter().find_map(|(channel, log)| {
            log.messages()
                .iter()
                .find(|message| message.id == id)
                .map(|message| (*channel, message))
        })
    }

    /// Everyone this client knows a name for, excluding the reader.
    ///
    /// Drawn from profiles rather than channel rosters so a person is
    /// reachable for a new conversation even when you share no channel with
    /// them yet — the only requirement is that their profile has loaded.
    pub fn people(&self, me: &PublicKey) -> Vec<(String, PublicKey)> {
        let mut out: Vec<(String, PublicKey)> = self
            .profiles
            .iter()
            .filter(|(pubkey, _)| *pubkey != me)
            .map(|(pubkey, profile)| (profile.name.clone(), *pubkey))
            .collect();
        out.sort_by_key(|(name, _)| name.to_lowercase());
        out.dedup_by(|(a, _), (b, _)| a == b);
        out
    }

    /// Resolves `@name` text into member pubkeys, via the SDK's matcher.
    ///
    /// Buzz sends mentions as `p` tags, not as text: an agent harness
    /// subscribes with `#p` set to its own key, so an `@name` the client never
    /// resolved does not reach it at all — the message simply looks unanswered.
    pub fn resolve_mentions(&self, content: &str) -> Vec<String> {
        let hexes: Vec<String> = self.profiles.keys().map(|pubkey| pubkey.to_hex()).collect();
        let profiles: Vec<MentionProfile<'_>> = self
            .profiles
            .values()
            .zip(hexes.iter())
            .map(|(profile, hex)| MentionProfile {
                pubkey: hex.as_str(),
                content_json: profile.content_json.as_str(),
            })
            .collect();
        let known: Vec<&str> = self
            .profiles
            .values()
            .map(|profile| profile.name.as_str())
            .collect();
        // The with-known variant, not the bare tokenizer: display names like
        // "Fizz Buzz" are two words and the bare one would only ever see "fizz".
        let names = extract_at_mentions_with_known(content, &known);
        match_names_to_profiles(&names, &profiles)
    }

    /// The name to show for a channel.
    ///
    /// Every DM the relay serves is named "DM", so ten conversations render as
    /// ten identical rows. For those, the participants are the name — everyone
    /// but the reader, since a DM list that repeats your own name back at you
    /// distinguishes nothing.
    pub fn channel_label(&self, channel: &Channel, me: &PublicKey) -> String {
        if channel.kind != ChannelKind::Dm {
            return channel.name.clone();
        }
        let others: Vec<String> = self
            .participants_of(channel)
            .iter()
            .filter(|pubkey| *pubkey != me)
            .map(|pubkey| self.display_name(pubkey))
            .collect();
        match others.len() {
            0 => channel.name.clone(),
            1..=2 => others.join(", "),
            // A group DM's full roster never fits the sidebar.
            _ => format!("{}, +{}", others[0], others.len() - 1),
        }
    }

    /// Every participant of every known channel, for a one-shot profile fetch.
    pub fn all_participants(&self) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        for channel in &self.channels {
            for pubkey in self.participants_of(channel) {
                seen.insert(pubkey.to_hex());
            }
        }
        for roster in self.members.values() {
            for pubkey in roster {
                seen.insert(pubkey.to_hex());
            }
        }
        seen.into_iter().collect()
    }

    /// Folds one event into state. Unknown kinds are ignored, not errors.
    pub fn apply(&mut self, event: &Event) {
        let kind = u32::from(event.kind.as_u16());
        match kind {
            KIND_PROFILE => self.apply_profile(event),
            KIND_NIP29_GROUP_METADATA => self.apply_channel_metadata(event),
            KIND_NIP29_GROUP_MEMBERS => self.apply_channel_members(event),
            k if CHANNEL_MESSAGE_KINDS.contains(&k) => self.apply_message(event),
            KIND_CANVAS => self.apply_canvas(event),
            KIND_REACTION => self.apply_reaction(event),
            KIND_TYPING_INDICATOR => self.apply_typing(event),
            KIND_STREAM_MESSAGE_EDIT => self.apply_edit(event),
            KIND_DELETION | KIND_NIP29_DELETE_EVENT => self.apply_delete(event),
            _ => {}
        }
    }

    fn apply_profile(&mut self, event: &Event) {
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&event.content) else {
            return;
        };
        let name = ["display_name", "name"]
            .iter()
            .find_map(|key| meta.get(*key).and_then(|v| v.as_str()))
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(name) = name {
            self.profiles.insert(
                event.pubkey,
                Profile {
                    name: name.to_string(),
                    content_json: event.content.clone(),
                },
            );
        }
    }

    fn apply_channel_metadata(&mut self, event: &Event) {
        let Some(id) = first_tag(event, "d").and_then(|v| Uuid::parse_str(&v).ok()) else {
            return;
        };
        let channel = Channel {
            id,
            name: first_tag(event, "name").unwrap_or_else(|| id.to_string()),
            topic: first_tag(event, "topic")
                .or_else(|| first_tag(event, "about"))
                .unwrap_or_default(),
            kind: match first_tag(event, "t").as_deref() {
                Some("stream") => ChannelKind::Stream,
                Some("forum") => ChannelKind::Forum,
                Some("dm") => ChannelKind::Dm,
                _ => ChannelKind::Other,
            },
            archived: first_tag(event, "archived").as_deref() == Some("true"),
            participants: all_tags(event, "p")
                .iter()
                .filter_map(|value| PublicKey::from_hex(value).ok())
                .collect(),
        };
        // 39000 is parameterized-replaceable: the relay only ever serves the
        // newest per `d`, so replacing in place is the whole update path.
        match self.channels.iter_mut().find(|c| c.id == id) {
            Some(existing) => *existing = channel,
            None => self.channels.push(channel),
        }
        self.channels
            .sort_by_key(|channel| channel.name.to_lowercase());
    }

    fn apply_channel_members(&mut self, event: &Event) {
        let Some(id) = first_tag(event, "d").and_then(|v| Uuid::parse_str(&v).ok()) else {
            return;
        };
        let members: Vec<PublicKey> = all_tags(event, "p")
            .iter()
            .filter_map(|value| PublicKey::from_hex(value).ok())
            .collect();
        // 39002 is parameterized-replaceable, so the newest roster is the whole
        // truth — merging with a previous one would resurrect removed members.
        self.members.insert(id, members);
    }

    fn apply_message(&mut self, event: &Event) {
        let Some(channel) = buzz_sdk::builders::extract_channel_id(event) else {
            return;
        };
        self.logs.entry(channel).or_default().insert(Message {
            id: event.id,
            author: event.pubkey,
            created_at: event.created_at.as_secs(),
            content: event.content.clone(),
            edited: false,
            mentions: all_tags(event, "p")
                .iter()
                .filter_map(|value| PublicKey::from_hex(value).ok())
                .collect(),
            root: thread_root(event),
        });
    }

    fn apply_canvas(&mut self, event: &Event) {
        let Some(channel) = buzz_sdk::builders::extract_channel_id(event) else {
            return;
        };
        let revision = Canvas {
            id: event.id,
            author: event.pubkey,
            updated_at: event.created_at.as_secs(),
            content: event.content.clone(),
        };
        // Newest wins. Not replaceable at the relay, so an older revision can
        // arrive after a newer one — during backfill, or from a slow peer.
        match self.canvases.get(&channel) {
            Some(existing) if existing.updated_at > revision.updated_at => {}
            _ => {
                self.canvases.insert(channel, revision);
            }
        }
    }

    /// The channel's current canvas, if one has been loaded.
    pub fn canvas(&self, channel: &Uuid) -> Option<&Canvas> {
        self.canvases.get(channel)
    }

    fn apply_reaction(&mut self, event: &Event) {
        let Some(target) = first_tag(event, "e").and_then(|id| EventId::from_hex(&id).ok()) else {
            return;
        };
        let emoji = display_emoji(event);
        if emoji.is_empty() {
            return;
        }
        // A reaction carries `h` only when the sender bothered; the relay
        // derives the channel from the `e` target either way. Falling back to
        // a scan means a reaction from a client that omitted it still lands.
        let reaction = Reaction {
            id: event.id,
            author: event.pubkey,
            emoji,
        };
        match buzz_sdk::builders::extract_channel_id(event) {
            Some(channel) => self
                .logs
                .entry(channel)
                .or_default()
                .add_reaction(target, reaction),
            None => {
                if let Some(log) = self
                    .logs
                    .values_mut()
                    .find(|log| log.items.iter().any(|message| message.id == target))
                {
                    log.add_reaction(target, reaction);
                }
            }
        }
    }

    fn apply_typing(&mut self, event: &Event) {
        let Some(channel) = buzz_sdk::builders::extract_channel_id(event) else {
            return;
        };
        self.logs
            .entry(channel)
            .or_default()
            .note_typing(event.pubkey, event.created_at.as_secs());
    }

    fn apply_edit(&mut self, event: &Event) {
        let Some(channel) = buzz_sdk::builders::extract_channel_id(event) else {
            return;
        };
        let Some(target) = first_tag(event, "e").and_then(|v| EventId::from_hex(&v).ok()) else {
            return;
        };
        self.logs.entry(channel).or_default().apply_edit(
            target,
            event.created_at.as_secs(),
            event.content.clone(),
        );
    }

    fn apply_delete(&mut self, event: &Event) {
        let targets: Vec<EventId> = event
            .tags
            .iter()
            .filter_map(|t| {
                let parts = t.as_slice();
                (parts.first().map(String::as_str) == Some("e"))
                    .then(|| parts.get(1))
                    .flatten()
                    .and_then(|v| EventId::from_hex(v.as_str()).ok())
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        // A tombstone carries the channel in its `h` tag, but a plain NIP-09
        // kind:5 from a third-party client may not. Falling back to every log
        // costs a scan and is the only way to honor those.
        match buzz_sdk::builders::extract_channel_id(event) {
            Some(channel) => {
                let log = self.logs.entry(channel).or_default();
                for target in targets {
                    log.apply_delete(target);
                    // A kind:5 targets a message or a reaction; the tag alone
                    // does not say which, so both are tried.
                    log.remove_reaction(target);
                }
            }
            None => {
                for log in self.logs.values_mut() {
                    for target in &targets {
                        log.apply_delete(*target);
                        log.remove_reaction(*target);
                    }
                }
            }
        }
    }
}

/// The NIP-10 thread root of an event, or `None` when it is top-level.
///
/// A bare `e` tag is not enough: mentions and quotes carry one too, so keying
/// on its presence treats nearly every message as a reply. The marker sits at
/// index 3 — `["e", id, "", "root"]`.
///
/// A direct reply to a top-level message carries only a `reply` marker (see
/// `thread_tags` in buzz-sdk), so that target *is* the root. A nested reply
/// carries both, and the explicit `root` wins.
fn thread_root(event: &Event) -> Option<EventId> {
    let marked = |want: &str| {
        event.tags.iter().find_map(|t| {
            let parts = t.as_slice();
            (parts.first().map(String::as_str) == Some("e")
                && parts.get(3).map(String::as_str) == Some(want))
            .then(|| parts.get(1))
            .flatten()
            .and_then(|id| EventId::from_hex(id.as_str()).ok())
        })
    };
    marked("root").or_else(|| marked("reply"))
}

fn all_tags(event: &Event, key: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let parts = t.as_slice();
            (parts.first().map(String::as_str) == Some(key))
                .then(|| parts.get(1).cloned())
                .flatten()
        })
        .collect()
}

/// What to draw for a reaction.
///
/// NIP-25 defines `+` and `-` as like and dislike rather than as literal
/// characters; a client that printed them would show a lone plus sign where
/// every other client shows a thumb. A custom emoji renders as its shortcode,
/// since a terminal cannot display the image it points at.
fn display_emoji(event: &Event) -> String {
    match event.content.trim() {
        "+" | "" => "👍".to_string(),
        "-" => "👎".to_string(),
        other => other.to_string(),
    }
}

fn first_tag(event: &Event, key: &str) -> Option<String> {
    event.tags.iter().find_map(|t| {
        let parts = t.as_slice();
        (parts.first().map(String::as_str) == Some(key))
            .then(|| parts.get(1).cloned())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    fn channel() -> Uuid {
        Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap()
    }

    fn message_at(keys: &Keys, created_at: u64, content: &str) -> Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([Tag::parse(["h", &channel().to_string()]).unwrap()])
            .custom_created_at(nostr::Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn messages_sort_by_time_then_id_not_arrival() {
        let keys = Keys::generate();
        let mut store = Store::default();
        // Same second, inserted in both orders across two runs of the fold.
        let a = message_at(&keys, 100, "a");
        let b = message_at(&keys, 100, "b");
        store.apply(&b);
        store.apply(&a);

        let mut other = Store::default();
        other.apply(&a);
        other.apply(&b);

        let order = |s: &Store| {
            s.log_or_empty(&channel())
                .messages()
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            order(&store),
            order(&other),
            "a tie on created_at must not resolve by arrival order"
        );
    }

    #[test]
    fn the_paging_cursor_is_the_oldest_message_held() {
        let keys = Keys::generate();
        let mut store = Store::default();
        store.apply(&message_at(&keys, 200, "later"));
        store.apply(&message_at(&keys, 100, "earlier"));
        let log = store.log_or_empty(&channel());
        assert_eq!(log.oldest_at(), Some(100));
        assert_eq!(log.newest_at(), Some(200));
    }

    #[test]
    fn re_requesting_the_boundary_second_neither_duplicates_nor_loses() {
        // `until` is inclusive in NIP-01, so paging from the oldest message
        // re-delivers its whole second. That overlap is deliberate: asking for
        // strictly-older would drop every message sharing a second with the
        // page boundary, and this client has no `before_id` cursor to key on
        // — that extension is bridge-only.
        let keys = Keys::generate();
        let boundary_a = message_at(&keys, 100, "boundary a");
        let boundary_b = message_at(&keys, 100, "boundary b");
        let older = message_at(&keys, 90, "older");

        let mut store = Store::default();
        store.apply(&boundary_a);
        assert_eq!(store.log_or_empty(&channel()).oldest_at(), Some(100));

        // The next page, requested with `until: 100`, returns the whole
        // boundary second again plus what precedes it.
        for event in [&boundary_a, &boundary_b, &older] {
            store.apply(event);
        }

        let contents: Vec<&str> = store
            .log_or_empty(&channel())
            .messages()
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(contents.len(), 3, "the redelivered message must not double");
        assert!(contents.contains(&"boundary b"), "a tie must not be lost");
        assert_eq!(contents[0], "older", "and the page lands in order");
    }

    #[test]
    fn a_duplicate_delivery_is_dropped() {
        let keys = Keys::generate();
        let mut store = Store::default();
        let event = message_at(&keys, 100, "once");
        store.apply(&event);
        store.apply(&event);
        assert_eq!(store.log_or_empty(&channel()).messages().len(), 1);
    }

    #[test]
    fn an_edit_arriving_before_its_target_still_applies() {
        // The ordinary case during backfill: pages come newest-first, so the
        // edit is seen before the message it rewrites.
        let keys = Keys::generate();
        let target = message_at(&keys, 100, "before");
        let edit = EventBuilder::new(Kind::Custom(40003), "after")
            .tags([
                Tag::parse(["h", &channel().to_string()]).unwrap(),
                Tag::parse(["e", &target.id.to_hex()]).unwrap(),
            ])
            .custom_created_at(nostr::Timestamp::from(200))
            .sign_with_keys(&keys)
            .unwrap();

        let mut store = Store::default();
        store.apply(&edit);
        store.apply(&target);

        let log = store.log_or_empty(&channel());
        assert_eq!(log.messages()[0].content, "after");
        assert!(log.messages()[0].edited);
    }

    #[test]
    fn an_older_edit_cannot_clobber_a_newer_one() {
        let keys = Keys::generate();
        let target = message_at(&keys, 100, "v0");
        let edit = |at: u64, body: &str| {
            EventBuilder::new(Kind::Custom(40003), body)
                .tags([
                    Tag::parse(["h", &channel().to_string()]).unwrap(),
                    Tag::parse(["e", &target.id.to_hex()]).unwrap(),
                ])
                .custom_created_at(nostr::Timestamp::from(at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let mut store = Store::default();
        store.apply(&target);
        store.apply(&edit(300, "newest"));
        store.apply(&edit(200, "older"));
        assert_eq!(
            store.log_or_empty(&channel()).messages()[0].content,
            "newest"
        );
    }

    #[test]
    fn a_tombstone_arriving_first_suppresses_the_message() {
        let keys = Keys::generate();
        let target = message_at(&keys, 100, "gone");
        let tombstone = EventBuilder::new(Kind::Custom(9005), "")
            .tags([
                Tag::parse(["h", &channel().to_string()]).unwrap(),
                Tag::parse(["e", &target.id.to_hex()]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .unwrap();

        let mut store = Store::default();
        store.apply(&tombstone);
        store.apply(&target);
        assert!(store.log_or_empty(&channel()).messages().is_empty());
    }

    #[test]
    fn channel_metadata_replaces_rather_than_duplicates() {
        let keys = Keys::generate();
        let meta = |name: &str| {
            EventBuilder::new(Kind::Custom(39000), "")
                .tags([
                    Tag::parse(["d", &channel().to_string()]).unwrap(),
                    Tag::parse(["name", name]).unwrap(),
                    Tag::parse(["t", "stream"]).unwrap(),
                ])
                .sign_with_keys(&keys)
                .unwrap()
        };
        let mut store = Store::default();
        store.apply(&meta("old"));
        store.apply(&meta("new"));
        assert_eq!(store.channels().len(), 1);
        assert_eq!(store.channels()[0].name, "new");
        assert_eq!(store.channels()[0].kind, ChannelKind::Stream);
    }

    #[test]
    fn an_unknown_kind_is_ignored_not_an_error() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(46001), "workflow step")
            .tags([Tag::parse(["h", &channel().to_string()]).unwrap()])
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = Store::default();
        store.apply(&event);
        assert!(store.log_or_empty(&channel()).messages().is_empty());
    }

    #[test]
    fn only_a_marked_e_tag_makes_a_message_a_reply() {
        // A mention or a quote carries a bare `e` tag too. Treating those as
        // replies pulls ordinary messages out of the channel timeline and
        // hides them inside threads nobody opened.
        let keys = Keys::generate();
        let target = message_at(&keys, 100, "root");
        let build = |tag: Tag| {
            EventBuilder::new(Kind::Custom(9), "body")
                .tags([Tag::parse(["h", &channel().to_string()]).unwrap(), tag])
                .sign_with_keys(&keys)
                .unwrap()
        };
        let quoted = build(Tag::parse(["e", &target.id.to_hex()]).unwrap());
        let replied = build(Tag::parse(["e", &target.id.to_hex(), "", "reply"]).unwrap());
        assert_eq!(thread_root(&quoted), None);
        assert_eq!(thread_root(&replied), Some(target.id));
    }

    #[test]
    fn a_nested_reply_is_rooted_at_the_root_not_its_parent() {
        // buzz-sdk emits both markers for a nested reply. Taking the parent
        // would scatter one conversation across several threads.
        let keys = Keys::generate();
        let root = message_at(&keys, 100, "root");
        let parent = message_at(&keys, 110, "parent");
        let nested = EventBuilder::new(Kind::Custom(9), "nested")
            .tags([
                Tag::parse(["h", &channel().to_string()]).unwrap(),
                Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
                Tag::parse(["e", &parent.id.to_hex(), "", "reply"]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .unwrap();
        assert_eq!(thread_root(&nested), Some(root.id));
    }

    #[test]
    fn replies_stay_out_of_the_channel_timeline() {
        // The relay's frozen contract: "replies never enter the channel
        // timeline". Rendering them flat is what made every message look
        // like a thread reply.
        let keys = Keys::generate();
        let root = message_at(&keys, 100, "question");
        let reply = EventBuilder::new(Kind::Custom(9), "answer")
            .tags([
                Tag::parse(["h", &channel().to_string()]).unwrap(),
                Tag::parse(["e", &root.id.to_hex(), "", "reply"]).unwrap(),
            ])
            .custom_created_at(nostr::Timestamp::from(200))
            .sign_with_keys(&keys)
            .unwrap();

        let mut store = Store::default();
        store.apply(&root);
        store.apply(&reply);
        let log = store.log_or_empty(&channel());

        let timeline: Vec<&str> = log
            .top_level()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(timeline, vec!["question"]);

        let thread: Vec<&str> = log
            .thread(root.id)
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            thread,
            vec!["question", "answer"],
            "root first, then replies"
        );

        assert_eq!(log.reply_counts().get(&root.id), Some(&1));
        assert_eq!(log.newest_thread(), Some(root.id));
    }

    #[test]
    fn a_thread_opens_even_when_its_root_predates_the_window() {
        // Replies can load while the root does not. Refusing to open then
        // leaves content visible in no view at all.
        let keys = Keys::generate();
        let absent = message_at(&keys, 1, "never applied");
        let reply = EventBuilder::new(Kind::Custom(9), "orphaned answer")
            .tags([
                Tag::parse(["h", &channel().to_string()]).unwrap(),
                Tag::parse(["e", &absent.id.to_hex(), "", "reply"]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = Store::default();
        store.apply(&reply);
        assert_eq!(store.log_or_empty(&channel()).thread(absent.id).len(), 1);
    }

    fn reaction(keys: &Keys, target: EventId, content: &str) -> Event {
        EventBuilder::new(Kind::Custom(7), content)
            .tags([
                Tag::parse(["h", &channel().to_string()]).unwrap(),
                Tag::parse(["e", &target.to_hex()]).unwrap(),
            ])
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn the_newest_canvas_revision_wins_whatever_order_it_arrives_in() {
        // Kind 40100 is not replaceable, so the relay keeps every save and an
        // older revision can arrive after a newer one — during backfill, or
        // from a slow peer.
        let keys = Keys::generate();
        let revision = |at: u64, body: &str| {
            EventBuilder::new(Kind::Custom(40100), body)
                .tags([Tag::parse(["h", &channel().to_string()]).unwrap()])
                .custom_created_at(nostr::Timestamp::from(at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let mut store = Store::default();
        store.apply(&revision(200, "newer"));
        store.apply(&revision(100, "older"));
        assert_eq!(store.canvas(&channel()).unwrap().content, "newer");
    }

    #[test]
    fn a_canvas_carries_the_revision_that_wrote_it() {
        // Without the revision id there is no way to notice someone saved
        // while your editor was open — 40100 has no compare-and-swap.
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(40100), "# notes")
            .tags([Tag::parse(["h", &channel().to_string()]).unwrap()])
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = Store::default();
        store.apply(&event);
        let canvas = store.canvas(&channel()).unwrap();
        assert_eq!(canvas.id, event.id);
        assert_eq!(canvas.author, keys.public_key());
    }

    #[test]
    fn a_canvas_belongs_only_to_its_own_channel() {
        let keys = Keys::generate();
        let other = Uuid::new_v4();
        let mut store = Store::default();
        store.apply(
            &EventBuilder::new(Kind::Custom(40100), "theirs")
                .tags([Tag::parse(["h", &other.to_string()]).unwrap()])
                .sign_with_keys(&keys)
                .unwrap(),
        );
        assert!(store.canvas(&channel()).is_none());
        assert!(store.canvas(&other).is_some());
    }

    #[test]
    fn reactions_collapse_by_emoji_and_mark_your_own() {
        let me = Keys::generate();
        let them = Keys::generate();
        let target = message_at(&them, 100, "ship it");
        let mut store = Store::default();
        store.apply(&target);
        store.apply(&reaction(&them, target.id, "👀"));
        store.apply(&reaction(&me, target.id, "👀"));
        store.apply(&reaction(&them, target.id, "🎉"));

        let groups = store
            .log_or_empty(&channel())
            .reactions(target.id, &me.public_key());
        assert_eq!(groups.len(), 2, "two kinds, not three reactions");
        assert_eq!(groups[0].emoji, "👀");
        assert_eq!(groups[0].count, 2);
        assert!(
            groups[0].mine.is_some(),
            "your own is the id a click removes"
        );
        assert_eq!(groups[1].emoji, "🎉");
        assert!(groups[1].mine.is_none());
    }

    #[test]
    fn a_plus_renders_as_a_thumb() {
        // NIP-25 defines `+` as like rather than as a literal character; a
        // client that printed it shows a lone plus where everyone else shows 👍.
        let keys = Keys::generate();
        let target = message_at(&keys, 100, "x");
        let mut store = Store::default();
        store.apply(&target);
        store.apply(&reaction(&keys, target.id, "+"));
        let groups = store
            .log_or_empty(&channel())
            .reactions(target.id, &Keys::generate().public_key());
        assert_eq!(groups[0].emoji, "👍");
    }

    #[test]
    fn one_reaction_of_each_kind_per_person() {
        // The optimistic local copy and the relay's echo are the same
        // reaction; counting both would show 2 for one person.
        let keys = Keys::generate();
        let target = message_at(&keys, 100, "x");
        let mut store = Store::default();
        store.apply(&target);
        let event = reaction(&keys, target.id, "🔥");
        store.apply(&event);
        store.apply(&event);
        assert_eq!(
            store
                .log_or_empty(&channel())
                .reactions(target.id, &Keys::generate().public_key())[0]
                .count,
            1
        );
    }

    #[test]
    fn deleting_a_reaction_takes_it_off_the_message() {
        let keys = Keys::generate();
        let target = message_at(&keys, 100, "x");
        let mut store = Store::default();
        store.apply(&target);
        let event = reaction(&keys, target.id, "🚀");
        store.apply(&event);
        store.apply(
            &EventBuilder::new(Kind::Custom(5), "")
                .tags([
                    Tag::parse(["h", &channel().to_string()]).unwrap(),
                    Tag::parse(["e", &event.id.to_hex()]).unwrap(),
                ])
                .sign_with_keys(&keys)
                .unwrap(),
        );
        assert!(store
            .log_or_empty(&channel())
            .reactions(target.id, &keys.public_key())
            .is_empty());
    }

    #[test]
    fn a_reaction_arriving_before_its_message_still_lands() {
        // Backfill pages newest-first, so this is the ordinary case rather
        // than an edge one.
        let keys = Keys::generate();
        let target = message_at(&keys, 100, "x");
        let mut store = Store::default();
        store.apply(&reaction(&keys, target.id, "👏"));
        store.apply(&target);
        assert_eq!(
            store
                .log_or_empty(&channel())
                .reactions(target.id, &Keys::generate().public_key())
                .len(),
            1
        );
    }

    #[test]
    fn a_typing_indicator_ages_out_on_its_own() {
        // There is no "stopped typing" event, so the TTL is the only thing
        // that ever takes one down.
        let keys = Keys::generate();
        let me = Keys::generate();
        let typing = EventBuilder::new(Kind::Custom(20002), "")
            .tags([Tag::parse(["h", &channel().to_string()]).unwrap()])
            .custom_created_at(nostr::Timestamp::from(1000))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = Store::default();
        store.apply(&typing);

        let log = store.log_or_empty(&channel());
        assert_eq!(log.typing(1000, &me.public_key()).len(), 1);
        assert_eq!(
            log.typing(1000 + TYPING_TTL_SECS, &me.public_key()).len(),
            0,
            "an indicator must not outlive its TTL"
        );
    }

    #[test]
    fn sending_a_message_stops_the_typing_indicator() {
        // Otherwise the sender stays "typing" for eight seconds after their
        // message is already on screen.
        let keys = Keys::generate();
        let me = Keys::generate();
        let mut store = Store::default();
        store.apply(
            &EventBuilder::new(Kind::Custom(20002), "")
                .tags([Tag::parse(["h", &channel().to_string()]).unwrap()])
                .custom_created_at(nostr::Timestamp::from(1000))
                .sign_with_keys(&keys)
                .unwrap(),
        );
        store.apply(&message_at(&keys, 1001, "there"));
        assert!(store
            .log_or_empty(&channel())
            .typing(1001, &me.public_key())
            .is_empty());
    }

    #[test]
    fn an_indicator_that_trails_its_own_message_is_ignored() {
        // A client's last indicator routinely arrives just after the message
        // it preceded; without suppression the sender flickers back to typing.
        let keys = Keys::generate();
        let me = Keys::generate();
        let mut store = Store::default();
        store.apply(&message_at(&keys, 1000, "sent"));
        store.apply(
            &EventBuilder::new(Kind::Custom(20002), "")
                .tags([Tag::parse(["h", &channel().to_string()]).unwrap()])
                .custom_created_at(nostr::Timestamp::from(1001))
                .sign_with_keys(&keys)
                .unwrap(),
        );
        assert!(store
            .log_or_empty(&channel())
            .typing(1001, &me.public_key())
            .is_empty());
    }

    #[test]
    fn your_own_typing_is_never_shown_back_to_you() {
        let me = Keys::generate();
        let mut store = Store::default();
        store.apply(
            &EventBuilder::new(Kind::Custom(20002), "")
                .tags([Tag::parse(["h", &channel().to_string()]).unwrap()])
                .custom_created_at(nostr::Timestamp::from(1000))
                .sign_with_keys(&me)
                .unwrap(),
        );
        assert!(store
            .log_or_empty(&channel())
            .typing(1000, &me.public_key())
            .is_empty());
    }

    #[test]
    fn unread_counts_only_what_arrived_after_the_frontier() {
        let me = Keys::generate();
        let them = Keys::generate();
        let mut store = Store::default();
        let msg = |keys: &Keys, at: u64, mentions: Option<&Keys>| {
            let mut tags = vec![Tag::parse(["h", &channel().to_string()]).unwrap()];
            if let Some(target) = mentions {
                tags.push(Tag::parse(["p", &target.public_key().to_hex()]).unwrap());
            }
            EventBuilder::new(Kind::Custom(9), "body")
                .tags(tags)
                .custom_created_at(nostr::Timestamp::from(at))
                .allow_self_tagging()
                .sign_with_keys(keys)
                .unwrap()
        };
        store.apply(&msg(&them, 100, None));
        store.apply(&msg(&them, 200, None));
        store.apply(&msg(&me, 300, None));

        let log = store.log_or_empty(&channel());
        assert_eq!(
            log.unread_after(0, &me.public_key()).0,
            2,
            "own messages never count"
        );
        assert_eq!(log.unread_after(150, &me.public_key()).0, 1);
        assert_eq!(log.unread_after(300, &me.public_key()).0, 0);
    }

    #[test]
    fn a_mention_is_reported_separately_from_the_count() {
        // "someone spoke" and "someone spoke to you" are different facts, and
        // a single number cannot carry both.
        let me = Keys::generate();
        let them = Keys::generate();
        let mut store = Store::default();
        let build = |mentions: bool, at: u64| {
            let mut tags = vec![Tag::parse(["h", &channel().to_string()]).unwrap()];
            if mentions {
                tags.push(Tag::parse(["p", &me.public_key().to_hex()]).unwrap());
            }
            EventBuilder::new(Kind::Custom(9), "body")
                .tags(tags)
                .custom_created_at(nostr::Timestamp::from(at))
                .sign_with_keys(&them)
                .unwrap()
        };
        store.apply(&build(false, 100));
        assert_eq!(
            store
                .log_or_empty(&channel())
                .unread_after(0, &me.public_key()),
            (1, false)
        );
        store.apply(&build(true, 200));
        assert_eq!(
            store
                .log_or_empty(&channel())
                .unread_after(0, &me.public_key()),
            (2, true)
        );
        // Reading past the mention clears it, not just the count.
        assert_eq!(
            store
                .log_or_empty(&channel())
                .unread_after(200, &me.public_key()),
            (0, false)
        );
    }

    #[test]
    fn a_dm_is_labelled_by_who_else_is_in_it() {
        // Every DM the relay serves is named "DM", so the name alone renders
        // every conversation as an identical row.
        let me = Keys::generate();
        let them = Keys::generate();
        // Discovery events are signed by the relay keypair, not by a
        // participant. That matters here rather than being incidental detail:
        // nostr 0.44 strips a `p` tag matching the signer, so a fixture signed
        // by one of the participants silently loses them from the roster.
        let relay = Keys::generate();
        let mut store = Store::default();
        store.apply(
            &EventBuilder::new(Kind::Custom(0), r#"{"display_name":"Samantha"}"#)
                .sign_with_keys(&them)
                .unwrap(),
        );
        store.apply(
            &EventBuilder::new(Kind::Custom(39000), "")
                .tags([
                    Tag::parse(["d", &channel().to_string()]).unwrap(),
                    Tag::parse(["name", "DM"]).unwrap(),
                    Tag::parse(["t", "dm"]).unwrap(),
                    Tag::parse(["p", &me.public_key().to_hex()]).unwrap(),
                    Tag::parse(["p", &them.public_key().to_hex()]).unwrap(),
                ])
                .sign_with_keys(&relay)
                .unwrap(),
        );
        let channel = &store.channels()[0];
        assert_eq!(store.channel_label(channel, &me.public_key()), "Samantha");
    }

    #[test]
    fn a_named_channel_keeps_its_name() {
        let keys = Keys::generate();
        let mut store = Store::default();
        store.apply(
            &EventBuilder::new(Kind::Custom(39000), "")
                .tags([
                    Tag::parse(["d", &channel().to_string()]).unwrap(),
                    Tag::parse(["name", "dev"]).unwrap(),
                    Tag::parse(["t", "stream"]).unwrap(),
                ])
                .sign_with_keys(&keys)
                .unwrap(),
        );
        assert_eq!(
            store.channel_label(&store.channels()[0], &keys.public_key()),
            "dev"
        );
    }

    #[test]
    fn display_name_prefers_profile_then_falls_back_to_hex() {
        let keys = Keys::generate();
        let mut store = Store::default();
        assert_eq!(store.display_name(&keys.public_key()).len(), 8);
        let profile = EventBuilder::new(Kind::Custom(0), r#"{"display_name":"Robert"}"#)
            .sign_with_keys(&keys)
            .unwrap();
        store.apply(&profile);
        assert_eq!(store.display_name(&keys.public_key()), "Robert");
    }
}
