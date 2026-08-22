//! Cross-device read state, per NIP-RS (`docs/nips/NIP-RS.md`).
//!
//! Read position lives in `kind:30078` addressable events that the user
//! publishes to themselves, NIP-44 encrypted to their own key. Every client
//! the user runs publishes its own coordinate; the effective frontier is the
//! componentwise `max()` across all of them — a grow-only register, so a
//! timestamp is only ever advanced, never lowered.
//!
//! That merge rule is why this is worth doing rather than counting unread
//! locally: reading a channel in Buzz Desktop advances the frontier there, and
//! this client picks it up. Without it, badges here disagree with the desktop
//! and stop meaning anything.
//!
//! # What this implements, and what it does not
//!
//! Frontier entries only. The spec's manual-unread override layer (`ov_*`
//! keys) is deliberately absent: it is durable state with its own eviction and
//! full-state-load obligations, and this client has no "mark as unread"
//! feature to justify them. Per the spec that is a conforming subset — a
//! client that neither reads nor writes `ov_*` may narrow its fetch by tag and
//! age, which is exactly what this does.
//!
//! Context identifiers are the channel UUID, matching the shapes NIP-RS names
//! as Buzz's own.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use nostr::nips::nip44;
use nostr::{EventBuilder, Keys, Kind, Tag};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Schema version of the encrypted blob. Blobs with any other value are
/// ignored rather than guessed at.
const BLOB_VERSION: u64 = 1;
/// Only frontiers from the last week are fetched. The spec allows this for
/// clients that do not carry override state, and it keeps the startup query
/// bounded on an account with a long history.
pub const HORIZON_SECS: u64 = 7 * 24 * 60 * 60;
/// Cap from the spec. Far above any real workspace, but a malformed blob is
/// not a reason to allocate without bound.
const MAX_CONTEXTS: usize = 10_000;

#[derive(Serialize, Deserialize)]
struct Blob {
    v: u64,
    client_id: String,
    contexts: HashMap<String, u64>,
}

/// The slot and client identifiers, persisted so this installation keeps one
/// coordinate across restarts rather than littering the relay with a new one
/// on every launch.
#[derive(Serialize, Deserialize)]
struct Identity {
    slot_id: String,
    client_id: String,
}

pub struct ReadState {
    slot_id: String,
    client_id: String,
    /// Merged effective frontier: context id → unix seconds.
    contexts: HashMap<String, u64>,
    /// Set when our own frontier has moved past what we last published.
    dirty: bool,
}

impl ReadState {
    /// Loads this installation's identifiers, creating them on first run.
    pub fn load() -> Self {
        let identity = read_identity().unwrap_or_else(|| {
            let identity = Identity {
                slot_id: random_slot_id(),
                client_id: format!("buzz-tui-{}", Uuid::new_v4()),
            };
            let _ = write_identity(&identity);
            identity
        });
        Self {
            slot_id: identity.slot_id,
            client_id: identity.client_id,
            contexts: HashMap::new(),
            dirty: false,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The `d` tag coordinate this client writes to.
    pub fn coordinate(&self) -> String {
        format!("read-state:{}", self.slot_id)
    }

    /// Everything at or before this timestamp in `channel` has been read.
    pub fn frontier(&self, channel: &Uuid) -> u64 {
        self.contexts
            .get(&channel.to_string())
            .copied()
            .unwrap_or(0)
    }

    /// Advances the frontier. Never lowers it — the merge is a grow-only
    /// register, and a client that moves a timestamp backwards would resurrect
    /// messages another device has already read.
    pub fn advance(&mut self, channel: &Uuid, at: u64) {
        let entry = self.contexts.entry(channel.to_string()).or_insert(0);
        if at > *entry {
            *entry = at;
            self.dirty = true;
        }
    }

    /// Folds one of our own `kind:30078` events into the effective state.
    ///
    /// Returns `true` when the event occupies this client's own coordinate but
    /// was written by a different installation — a conflict, after which the
    /// caller must not publish to that coordinate again.
    pub fn merge(&mut self, event: &nostr::Event, keys: &Keys) -> bool {
        let Some(slot) = coordinate_slot(event) else {
            return false;
        };
        // The `t` tag is a discoverability hint, not a relay guarantee: kind
        // 30078 is shared with unrelated app data, so it is re-checked here.
        if !has_read_state_tag(event) {
            return false;
        }
        let Ok(plaintext) = nip44::decrypt(keys.secret_key(), &keys.public_key(), &event.content)
        else {
            return false;
        };
        let Ok(blob) = serde_json::from_str::<Blob>(&plaintext) else {
            return false;
        };
        if blob.v != BLOB_VERSION || blob.client_id.is_empty() || blob.client_id.len() > 64 {
            return false;
        }
        if blob.contexts.len() > MAX_CONTEXTS {
            return false;
        }

        for (context, at) in blob.contexts {
            // Override entries belong to a layer this client does not
            // implement; carrying them into the frontier map would treat a
            // counter as a timestamp.
            if context.starts_with("ov_") || context.len() > 256 {
                continue;
            }
            let context = context.strip_prefix("esc:").unwrap_or(&context).to_string();
            let entry = self.contexts.entry(context).or_insert(0);
            *entry = (*entry).max(at);
        }

        slot == self.slot_id && blob.client_id != self.client_id
    }

    /// Abandons a conflicted coordinate for a fresh one.
    pub fn rotate_slot(&mut self) {
        self.slot_id = random_slot_id();
        let _ = write_identity(&Identity {
            slot_id: self.slot_id.clone(),
            client_id: self.client_id.clone(),
        });
        self.dirty = true;
    }

    /// Builds this client's coordinate for publishing.
    pub fn build(&mut self, keys: &Keys) -> Result<EventBuilder> {
        let blob = Blob {
            v: BLOB_VERSION,
            client_id: self.client_id.clone(),
            contexts: self
                .contexts
                .iter()
                .map(|(context, at)| (escape(context), *at))
                .collect(),
        };
        let plaintext = serde_json::to_string(&blob).context("serializing read state")?;
        let content = nip44::encrypt(
            keys.secret_key(),
            &keys.public_key(),
            plaintext,
            nip44::Version::V2,
        )
        .context("encrypting read state")?;

        let tags = vec![
            Tag::parse(["d", &self.coordinate()]).context("d tag")?,
            Tag::parse(["t", "read-state"]).context("t tag")?,
        ];
        self.dirty = false;
        Ok(EventBuilder::new(Kind::Custom(30078), content).tags(tags))
    }
}

/// Escapes a context id that would otherwise collide with the reserved
/// override namespace. Buzz's own shapes never trigger it; the rule is
/// implemented so a context from elsewhere cannot forge an `ov_` key.
fn escape(context: &str) -> String {
    if context.starts_with("ov_") || context.starts_with("esc:") {
        format!("esc:{context}")
    } else {
        context.to_string()
    }
}

/// The slot id from a well-formed `read-state:<32 hex>` coordinate.
///
/// The shape is fixed by the spec so a relay can recognize the coordinate
/// structurally; anything else is not a read-state event.
fn coordinate_slot(event: &nostr::Event) -> Option<String> {
    let mut d_tags = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("d"));
    let first = d_tags.next()?;
    if d_tags.next().is_some() {
        return None;
    }
    let slot = first.as_slice().get(1)?.strip_prefix("read-state:")?;
    (slot.len() == 32
        && slot
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()))
    .then(|| slot.to_string())
}

fn has_read_state_tag(event: &nostr::Event) -> bool {
    event
        .tags
        .iter()
        .filter(|tag| {
            let parts = tag.as_slice();
            parts.first().map(String::as_str) == Some("t")
                && parts.get(1).map(String::as_str) == Some("read-state")
        })
        .count()
        == 1
}

fn random_slot_id() -> String {
    // Two UUIDs' worth of entropy trimmed to the 32 hex characters the
    // coordinate shape requires.
    Uuid::new_v4().simple().to_string()
}

fn identity_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("buzz-tui").join("identity.json"))
}

fn read_identity() -> Option<Identity> {
    let raw = std::fs::read_to_string(identity_path()?).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_identity(identity: &Identity) -> Result<()> {
    let path = identity_path().context("no config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating config directory")?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(identity)?).context("writing identity")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> Uuid {
        Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap()
    }

    fn state(client_id: &str) -> ReadState {
        ReadState {
            slot_id: random_slot_id(),
            client_id: client_id.to_string(),
            contexts: HashMap::new(),
            dirty: false,
        }
    }

    #[test]
    fn a_frontier_only_ever_advances() {
        // Grow-only register. Lowering one would resurrect messages another
        // device has already read.
        let mut read = state("a");
        read.advance(&channel(), 200);
        read.advance(&channel(), 100);
        assert_eq!(read.frontier(&channel()), 200);
    }

    #[test]
    fn advancing_to_the_same_time_does_not_dirty_the_blob() {
        // Otherwise every redraw republishes an identical event.
        let mut read = state("a");
        read.advance(&channel(), 200);
        let mut read = ReadState {
            dirty: false,
            ..read
        };
        read.advance(&channel(), 200);
        assert!(!read.is_dirty());
    }

    #[test]
    fn a_round_trip_through_a_signed_event_preserves_the_frontier() {
        let keys = Keys::generate();
        let mut writer = state("writer");
        writer.advance(&channel(), 1_700_000_000);
        let event = writer.build(&keys).unwrap().sign_with_keys(&keys).unwrap();

        let mut reader = state("reader");
        assert!(
            !reader.merge(&event, &keys),
            "a foreign slot is not a conflict"
        );
        assert_eq!(reader.frontier(&channel()), 1_700_000_000);
    }

    #[test]
    fn merging_takes_the_newer_timestamp_from_either_side() {
        let keys = Keys::generate();
        let mut desktop = state("desktop");
        desktop.advance(&channel(), 500);
        let event = desktop.build(&keys).unwrap().sign_with_keys(&keys).unwrap();

        let mut here = state("here");
        here.advance(&channel(), 900);
        here.merge(&event, &keys);
        assert_eq!(here.frontier(&channel()), 900, "ours was newer");

        let mut behind = state("behind");
        behind.advance(&channel(), 100);
        behind.merge(&event, &keys);
        assert_eq!(behind.frontier(&channel()), 500, "theirs was newer");
    }

    #[test]
    fn our_own_coordinate_written_by_another_client_is_a_conflict() {
        // The spec's rule: publishing over it would clobber another
        // installation's state, so the caller must rotate to a new slot.
        //
        // Deliberately does not call `rotate_slot`: that persists, and a unit
        // test has no business writing to the user's config directory. The
        // rotation itself is covered below.
        let keys = Keys::generate();
        let mut other = state("someone-else");
        other.advance(&channel(), 100);
        let event = other.build(&keys).unwrap().sign_with_keys(&keys).unwrap();

        let mut ours = ReadState {
            slot_id: other.slot_id.clone(),
            client_id: "a-different-installation".into(),
            contexts: HashMap::new(),
            dirty: false,
        };
        assert!(ours.merge(&event, &keys));
    }

    #[test]
    fn a_rotated_slot_is_a_different_coordinate() {
        let first = random_slot_id();
        let second = random_slot_id();
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn a_malformed_coordinate_is_ignored() {
        let keys = Keys::generate();
        let build = |d: &str| {
            EventBuilder::new(Kind::Custom(30078), "x")
                .tags([
                    Tag::parse(["d", d]).unwrap(),
                    Tag::parse(["t", "read-state"]).unwrap(),
                ])
                .sign_with_keys(&keys)
                .unwrap()
        };
        assert_eq!(coordinate_slot(&build("read-state:short")), None);
        assert_eq!(coordinate_slot(&build("something-else")), None);
        assert_eq!(
            coordinate_slot(&build("read-state:ABCDEF01234567890ABCDEF012345678")),
            None,
            "uppercase is not the fixed shape"
        );
        let good = random_slot_id();
        assert_eq!(
            coordinate_slot(&build(&format!("read-state:{good}"))),
            Some(good)
        );
    }

    #[test]
    fn an_event_without_the_read_state_tag_is_not_read_state() {
        // kind 30078 is shared with unrelated application data.
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(30078), "x")
            .tags([Tag::parse(["d", &format!("read-state:{}", random_slot_id())]).unwrap()])
            .sign_with_keys(&keys)
            .unwrap();
        let mut read = state("a");
        assert!(!read.merge(&event, &keys));
        assert!(read.contexts.is_empty());
    }

    /// Encrypts a hand-built blob, bypassing `build`'s escaping so wire-level
    /// shapes can be tested directly.
    fn raw_blob(keys: &Keys, slot: &str, contexts: &[(&str, u64)]) -> nostr::Event {
        let blob = serde_json::json!({
            "v": 1,
            "client_id": "someone-else",
            "contexts": contexts
                .iter()
                .map(|(key, at)| (key.to_string(), *at))
                .collect::<HashMap<String, u64>>(),
        });
        let content = nip44::encrypt(
            keys.secret_key(),
            &keys.public_key(),
            blob.to_string(),
            nip44::Version::V2,
        )
        .unwrap();
        EventBuilder::new(Kind::Custom(30078), content)
            .tags([
                Tag::parse(["d", &format!("read-state:{slot}")]).unwrap(),
                Tag::parse(["t", "read-state"]).unwrap(),
            ])
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn override_counters_are_not_mistaken_for_frontiers() {
        // A client that implements the override layer puts `ov_c:` keys in the
        // same map. Their values are counters, not timestamps: folding one in
        // would mark a channel read up to an arbitrary integer.
        let keys = Keys::generate();
        let event = raw_blob(
            &keys,
            &random_slot_id(),
            &[
                ("ov_c:some-context", 7),
                ("ov_s:some-context", 3),
                (&channel().to_string(), 400),
            ],
        );

        let mut reader = state("reader");
        reader.merge(&event, &keys);
        assert_eq!(reader.frontier(&channel()), 400);
        assert!(
            !reader.contexts.keys().any(|key| key.starts_with("ov_")),
            "override counters must not enter the frontier map"
        );
    }

    #[test]
    fn an_escaped_context_recovers_its_raw_id() {
        // The escape exists so a legitimate context whose id happens to start
        // with `ov_` survives the round trip instead of being dropped as an
        // override counter.
        let keys = Keys::generate();
        let event = raw_blob(&keys, &random_slot_id(), &[("esc:ov_s:legit", 900)]);
        let mut reader = state("reader");
        reader.merge(&event, &keys);
        assert_eq!(reader.contexts.get("ov_s:legit"), Some(&900));
    }

    #[test]
    fn a_blob_with_an_unknown_schema_version_is_ignored() {
        let keys = Keys::generate();
        let content = nip44::encrypt(
            keys.secret_key(),
            &keys.public_key(),
            serde_json::json!({"v": 99, "client_id": "x", "contexts": {"c": 1}}).to_string(),
            nip44::Version::V2,
        )
        .unwrap();
        let event = EventBuilder::new(Kind::Custom(30078), content)
            .tags([
                Tag::parse(["d", &format!("read-state:{}", random_slot_id())]).unwrap(),
                Tag::parse(["t", "read-state"]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .unwrap();
        let mut reader = state("reader");
        reader.merge(&event, &keys);
        assert!(reader.contexts.is_empty());
    }

    #[test]
    fn a_reserved_context_id_survives_escaping_intact() {
        assert_eq!(escape("ov_s:evil"), "esc:ov_s:evil");
        assert_eq!(escape("esc:foo"), "esc:esc:foo");
        // Buzz's own shapes are untouched.
        assert_eq!(escape(&channel().to_string()), channel().to_string());
    }
}
