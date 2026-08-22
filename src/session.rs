//! The relay session: one authenticated socket, many multiplexed subscriptions.
//!
//! `buzz-ws-client` owns the wire format and the NIP-42 handshake but is
//! request/response shaped — one caller, `next_event` off a buffer, no
//! subscription bookkeeping and no reconnect. That is the right shape for
//! `buzz-cli`'s one-shot publishes and the wrong one for a client that must
//! read and write concurrently for hours. This module is the missing half.
//!
//! It is a close port of `desktop/src-tauri/src/native_relay_client.rs`, which
//! solved the same problem for the desktop backend but lives in a crate the
//! workspace excludes. The invariants below are that module's, kept verbatim
//! where they still hold; the desktop's archive-ownership and session-leasing
//! machinery is dropped, since a TUI has exactly one consumer.
//!
//! # Two rules the rest of the app depends on
//!
//! 1. **A subscription id's filter is immutable for the life of the session.**
//!    To change a filter, use a new id. A `CLOSED` frame carries only the id,
//!    so a rejection caused by the old filter is indistinguishable from one
//!    caused by the new — backoff would latch onto a subscription that never
//!    failed.
//! 2. **This task never renders.** The relay pushes with `try_send` and drops
//!    a connection after three consecutive full buffers, so a slow paint on
//!    the socket task is a disconnect. Events leave here through a channel and
//!    are drawn somewhere else.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Event, Keys, Tag};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Backoff floor for reconnect attempts.
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(500);
/// Backoff ceiling for reconnect attempts.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
/// How long a read may block before the loop re-checks cancellation. Not a
/// connection timeout: an idle relay is normal, so a lapsed read just loops.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Backoff floor for reopening a subscription the relay CLOSED.
const CLOSED_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
/// Backoff ceiling for reopening a CLOSED subscription.
const CLOSED_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
/// Delay for a `rate-limited:` CLOSED that carries no `retry in Ns` hint.
/// Matches the default both existing Buzz clients agree on.
const CLOSED_RATE_LIMIT_DEFAULT: Duration = Duration::from_secs(10);
/// Frames this session may write in one burst.
///
/// The relay admits `human_ws_events_per_sec` (10) × a 5-second window = 50
/// EVENT/REQ/COUNT frames per 5s per pubkey. That is a *window*, not a
/// per-second cap: a burst well inside it is fine, which matters because
/// startup legitimately needs a dozen frames at once — two live subscriptions
/// plus a REQ and a CLOSE for each of the channel, history and profile
/// queries. Pacing every one of those behind a fixed delay serializes the
/// whole bootstrap and the app appears to hang.
const FRAME_BURST: f64 = 36.0;
/// Sustained refill, in frames per second. Below the relay's 10/s so a client
/// that keeps writing indefinitely stays inside the window.
const FRAME_REFILL_PER_SEC: f64 = 8.0;

/// A live subscription: a caller-stable id plus the filter it opens with.
#[derive(Clone, Debug)]
pub struct Subscription {
    /// Reused verbatim as the relay subscription id, so a resubscribe after
    /// reconnect replaces rather than duplicates. See the module docs for why
    /// this may not be reused with a different filter.
    pub id: String,
    pub filter: Value,
}

/// What the session reports upward. The UI demultiplexes on `subscription_id`.
#[derive(Debug)]
pub enum SessionEvent {
    Connected,
    Disconnected(String),
    Event {
        subscription_id: String,
        event: Box<Event>,
    },
    Eose {
        subscription_id: String,
    },
    Notice(String),
}

struct PendingRequest {
    events: Vec<Event>,
    complete: oneshot::Sender<Result<Vec<Event>, String>>,
}

struct Outgoing {
    event: Event,
    reply: oneshot::Sender<Result<(), String>>,
}

/// Desired set plus the write-time record of what has left it.
///
/// One lock covers both because reconcile must read them together: snapshotting
/// the desired set and draining `removed` in separate acquisitions lets a
/// `set_subscriptions` land in the gap, so the drain would be spent against a
/// stale snapshot and could reopen a subscription the caller just dropped.
#[derive(Default)]
struct SessionState {
    desired: Vec<Subscription>,
    transient: Vec<Subscription>,
    /// Ids whose exact subscription has left `desired` since the last
    /// reconcile drained this. Recorded at write time because reconcile cannot
    /// derive it: wakes coalesce, so a remove followed by a re-add is observed
    /// as a single pass whose desired set never lost the id.
    removed: HashSet<String>,
}

impl SessionState {
    /// Installs a new desired set, recording every departure.
    fn replace_desired(&mut self, subscriptions: Vec<Subscription>) {
        for previous in std::mem::replace(&mut self.desired, subscriptions) {
            // Departure is keyed on the exact subscription, not the id alone:
            // the relay replaces by id, so a changed filter retires the old
            // subscription just as surely as dropping the id would, and its
            // backoff must not be inherited.
            let survivor = self.desired.iter().find(|next| next.id == previous.id);
            if survivor.is_some_and(|next| next.filter == previous.filter) {
                continue;
            }
            self.removed.insert(previous.id);
        }
    }
}

pub struct RelaySession {
    state: Mutex<SessionState>,
    requests: Mutex<HashMap<String, PendingRequest>>,
    outbox: Mutex<Vec<Outgoing>>,
    wake: mpsc::Sender<()>,
    cancel: CancellationToken,
}

impl RelaySession {
    /// Starts a session against `relay_url`, authenticating as `keys`.
    ///
    /// The socket reconnects with exponential backoff and resubscribes the
    /// *current* desired set — never a snapshot captured at connect time, so a
    /// subscription change during an outage is honored by the reconnect.
    /// `auth_tag` is the NIP-OA owner attestation. A membership-gated relay
    /// rejects a managed-agent key without it — `restricted: not a relay
    /// member` — so an agent identity cannot connect at all unless it is
    /// carried on the kind:22242 challenge response.
    pub fn start(
        relay_url: String,
        keys: Keys,
        auth_tag: Option<Tag>,
    ) -> (Arc<RelaySession>, mpsc::Receiver<SessionEvent>) {
        let (wake, wake_rx) = mpsc::channel(1);
        let (events_tx, events_rx) = mpsc::channel(512);
        let session = Arc::new(RelaySession {
            state: Mutex::new(SessionState::default()),
            requests: Mutex::new(HashMap::new()),
            outbox: Mutex::new(Vec::new()),
            wake,
            cancel: CancellationToken::new(),
        });
        tokio::spawn(run_session(
            relay_url,
            keys,
            auth_tag,
            Arc::clone(&session),
            wake_rx,
            events_tx,
        ));
        (session, events_rx)
    }

    /// Replaces the desired subscription set and wakes the loop to reconcile.
    ///
    /// Reconciliation is declarative rather than incremental: callers state
    /// what they want and the loop diffs. An incremental add/remove API would
    /// have to be replayed in order across a reconnect, which is exactly the
    /// bug class this avoids.
    pub async fn set_subscriptions(&self, subscriptions: Vec<Subscription>) {
        self.state.lock().await.replace_desired(subscriptions);
        // A full channel already means "reconcile pending", so a failed send
        // is success: the loop has not yet consumed the previous wake.
        let _ = self.wake.try_send(());
    }

    /// Runs one finite query to EOSE without disturbing live subscriptions.
    ///
    /// The id is fresh every call, so CLOSED history can never leak between
    /// pages or into a long-lived subscription.
    pub async fn fetch(&self, filter: Value, timeout: Duration) -> Result<Vec<Event>, String> {
        let id = format!("fetch-{}", uuid::Uuid::new_v4());
        let (complete, result) = oneshot::channel();
        self.requests.lock().await.insert(
            id.clone(),
            PendingRequest {
                events: Vec::new(),
                complete,
            },
        );
        self.state.lock().await.transient.push(Subscription {
            id: id.clone(),
            filter,
        });
        let _ = self.wake.try_send(());

        let outcome = tokio::select! {
            _ = self.cancel.cancelled() => Err("session cancelled".to_string()),
            value = tokio::time::timeout(timeout, result) => match value {
                Ok(Ok(value)) => value,
                Ok(Err(_)) => Err("request ended before EOSE".to_string()),
                Err(_) => Err("request timed out".to_string()),
            }
        };
        self.finish_request(&id).await;
        outcome
    }

    /// Publishes a signed event and resolves when the relay's OK arrives.
    pub async fn publish(&self, event: Event, timeout: Duration) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.outbox.lock().await.push(Outgoing { event, reply });
        let _ = self.wake.try_send(());
        tokio::select! {
            _ = self.cancel.cancelled() => Err("session cancelled".to_string()),
            value = tokio::time::timeout(timeout, result) => match value {
                Ok(Ok(value)) => value,
                Ok(Err(_)) => Err("connection dropped before OK".to_string()),
                Err(_) => Err("publish timed out".to_string()),
            }
        }
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    async fn finish_request(&self, id: &str) {
        self.requests.lock().await.remove(id);
        let mut state = self.state.lock().await;
        state.transient.retain(|subscription| subscription.id != id);
        state.removed.insert(id.to_string());
        drop(state);
        let _ = self.wake.try_send(());
    }
}

async fn run_session(
    relay_url: String,
    keys: Keys,
    auth_tag: Option<Tag>,
    session: Arc<RelaySession>,
    mut wake_rx: mpsc::Receiver<()>,
    events: mpsc::Sender<SessionEvent>,
) {
    let mut delay = RECONNECT_BASE_DELAY;
    loop {
        if session.cancel.is_cancelled() {
            return;
        }

        match NostrWsConnection::connect_authenticated(&relay_url, &keys, auth_tag.as_ref()).await {
            Ok(conn) => {
                // A connection that authenticated is healthy regardless of how
                // long it then lived, so backoff resets here rather than on
                // clean exit — a socket that drops after one event must not
                // inherit the previous failure's delay.
                delay = RECONNECT_BASE_DELAY;
                let _ = events.send(SessionEvent::Connected).await;
                run_connection(conn, &session, &mut wake_rx, &events).await;
                let _ = events
                    .send(SessionEvent::Disconnected("socket closed".into()))
                    .await;
            }
            Err(error) => {
                let _ = events
                    .send(SessionEvent::Disconnected(error.to_string()))
                    .await;
            }
        }

        if session.cancel.is_cancelled() {
            return;
        }
        tokio::select! {
            _ = session.cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

/// Drives one connected socket until it drops or the session is cancelled.
async fn run_connection(
    mut conn: NostrWsConnection,
    session: &RelaySession,
    wake_rx: &mut mpsc::Receiver<()>,
    events: &mpsc::Sender<SessionEvent>,
) {
    // Subscription ids currently open ON THIS SOCKET. Deliberately local: a new
    // socket has none, so reconnect resubscribes the full desired set without
    // any explicit "resubscribe" path that could drift from the normal one.
    let mut open: HashMap<String, Value> = HashMap::new();
    // Reopen schedule for ids the relay CLOSED, equally per-socket. An entry is
    // valid only while its id has been continuously desired since the CLOSED
    // that created it, which makes eviction the whole design: a delivered
    // event, an EOSE, a departure from the desired set, or a dropped socket.
    let mut retries: HashMap<String, ClosedRetry> = HashMap::new();
    // OKs we are still waiting on, per-socket for the same reason: a dropped
    // connection can never answer them, so they fail with it.
    let mut pending_ok: HashMap<String, oneshot::Sender<Result<(), String>>> = HashMap::new();
    // When the next paced write may go out, if a pass left work behind.
    let mut pace_at: Option<Instant>;
    let mut budget = FrameBudget::new();

    macro_rules! pass {
        () => {
            match reconcile(
                &mut conn,
                session,
                &mut open,
                &mut retries,
                &mut pending_ok,
                &mut budget,
            )
            .await
            {
                Pass::Failed => return,
                Pass::More => pace_at = Some(budget.next_available()),
                Pass::Done => pace_at = None,
            }
        };
    }

    pass!();

    loop {
        // Earliest pending reopen or paced write, whichever comes first.
        // `None` disables the arm rather than sleeping on a far-future
        // instant, so an idle connection never wakes on this branch.
        let retry_at = retries.values().filter_map(|retry| retry.due_at).min();
        let wake_at = [retry_at, pace_at].into_iter().flatten().min();

        tokio::select! {
            _ = session.cancel.cancelled() => {
                let _ = conn.disconnect().await;
                return;
            }
            Some(()) = wake_rx.recv() => {
                // A wake may arrive mid-pacing. Honour the interval rather
                // than letting a burst of caller changes bypass it.
                if pace_at.is_none() {
                    pass!();
                }
            }
            // The edge that makes a CLOSED recoverable. Without it, nothing
            // re-enters reconcile unless the desired set changes again, and for
            // a stable set that means the subscription is dead for the life of
            // the socket.
            _ = tokio::time::sleep_until(wake_at.unwrap_or_else(Instant::now)),
                if wake_at.is_some() =>
            {
                for retry in retries.values_mut() {
                    if retry.due_at.is_some_and(|due| due <= Instant::now()) {
                        retry.due_at = None;
                    }
                }
                pass!();
            }
            message = conn.next_event(READ_TIMEOUT) => {
                match message {
                    Ok(RelayMessage::Event { subscription_id, event }) => {
                        // A CLOSE races in flight with events already queued at
                        // the relay, so this is the last line of defense against
                        // delivering out-of-scope events after a change.
                        if !open.contains_key(&subscription_id) {
                            continue;
                        }
                        let is_request = session.requests.lock().await.contains_key(&subscription_id);
                        if is_request {
                            // Reject forged events before retaining them,
                            // bounding memory at the transport seam.
                            if event.verify().is_err() {
                                continue;
                            }
                            if let Some(request) =
                                session.requests.lock().await.get_mut(&subscription_id)
                            {
                                request.events.push(*event);
                            }
                            continue;
                        }
                        // Delivery proves the subscription is healthy, so any
                        // accumulated backoff for it is stale.
                        retries.remove(&subscription_id);
                        if event.verify().is_err() {
                            continue;
                        }
                        if events
                            .send(SessionEvent::Event { subscription_id, event })
                            .await
                            .is_err()
                        {
                            // The UI is gone; nothing left to serve.
                            let _ = conn.disconnect().await;
                            return;
                        }
                    }
                    Ok(RelayMessage::Ok(ok)) => {
                        if let Some(reply) = pending_ok.remove(&ok.event_id) {
                            let _ = reply.send(if ok.accepted {
                                Ok(())
                            } else {
                                Err(ok.message.clone())
                            });
                        }
                    }
                    Ok(RelayMessage::Closed { subscription_id, message }) => {
                        // A CLOSED for a subscription this socket is not running
                        // is stale — our own CLOSE raced it. Minting retry state
                        // from it would resurrect an entry nothing can evict.
                        if open.remove(&subscription_id).is_none() {
                            continue;
                        }
                        if let Some(request) =
                            session.requests.lock().await.remove(&subscription_id)
                        {
                            let _ = request
                                .complete
                                .send(Err(format!("relay closed request: {message}")));
                            let mut state = session.state.lock().await;
                            state.transient.retain(|s| s.id != subscription_id);
                            state.removed.insert(subscription_id.clone());
                            drop(state);
                            let _ = session.wake.try_send(());
                            continue;
                        }
                        let retry = retries.entry(subscription_id.clone()).or_default();
                        retry.schedule(&message);
                        let _ = events
                            .send(SessionEvent::Notice(format!(
                                "closed {subscription_id}: {message}"
                            )))
                            .await;
                    }
                    Ok(RelayMessage::Eose { subscription_id }) => {
                        // The relay served this subscription, so whatever caused
                        // an earlier CLOSED has cleared. This is what keeps an
                        // intermittent relay from ratcheting to the 30s ceiling
                        // and staying there.
                        let was_open = open.contains_key(&subscription_id);
                        if let Some(request) =
                            session.requests.lock().await.remove(&subscription_id)
                        {
                            let _ = request.complete.send(Ok(request.events));
                            let mut state = session.state.lock().await;
                            state.transient.retain(|s| s.id != subscription_id);
                            state.removed.insert(subscription_id.clone());
                            drop(state);
                            let _ = session.wake.try_send(());
                            continue;
                        }
                        retries.remove(&subscription_id);
                        // The relay is running a subscription this socket does
                        // not think is open, so the two disagree. EOSE is the
                        // fence that makes this recoverable: frames on one
                        // socket are ordered, so a stale CLOSED from a previous
                        // generation necessarily precedes the recreated
                        // generation's EOSE.
                        if !was_open {
                            let _ = session.wake.try_send(());
                        }
                        let _ = events.send(SessionEvent::Eose { subscription_id }).await;
                    }
                    Ok(RelayMessage::Notice { message }) => {
                        let _ = events.send(SessionEvent::Notice(message)).await;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        if !is_read_timeout(&error) {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Token bucket over the relay's admission window.
///
/// Bursty by design: the relay measures a 5-second window, so spending the
/// bucket at once and refilling steadily matches what it actually enforces.
/// A fixed inter-frame delay does not — it is strictly slower for the same
/// safety, and the difference is the entire startup latency.
struct FrameBudget {
    tokens: f64,
    last: Instant,
}

impl FrameBudget {
    fn new() -> Self {
        Self {
            tokens: FRAME_BURST,
            last: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * FRAME_REFILL_PER_SEC).min(FRAME_BURST);
        self.last = now;
    }

    /// Spends one token, or reports that the caller must wait.
    fn take(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// When the next token lands, for scheduling the follow-up pass.
    fn next_available(&self) -> Instant {
        let deficit = (1.0 - self.tokens).max(0.0);
        Instant::now() + Duration::from_secs_f64(deficit / FRAME_REFILL_PER_SEC)
    }
}

/// Outcome of one reconcile pass.
enum Pass {
    /// Everything the caller wanted is on the wire.
    Done,
    /// Work remains; the caller must schedule another pass after the pacing
    /// interval rather than looping immediately.
    More,
    /// The socket failed and the caller should reconnect.
    Failed,
}

/// Brings the socket's open subscriptions in line with the desired set and
/// flushes the outbox, writing at most [`FRAMES_PER_PASS`] frames.
async fn reconcile(
    conn: &mut NostrWsConnection,
    session: &RelaySession,
    open: &mut HashMap<String, Value>,
    retries: &mut HashMap<String, ClosedRetry>,
    pending_ok: &mut HashMap<String, oneshot::Sender<Result<(), String>>>,
    budget: &mut FrameBudget,
) -> Pass {
    // Snapshot and drain in ONE acquisition. Taking them separately would let a
    // `set_subscriptions` land in the gap, spending its removal against a
    // desired set captured before it.
    let (desired, removed) = {
        let mut state = session.state.lock().await;
        let removed = std::mem::take(&mut state.removed);
        (
            state
                .desired
                .iter()
                .chain(&state.transient)
                .cloned()
                .collect::<Vec<_>>(),
            removed,
        )
    };

    // Retry state is only valid while its id has been continuously desired
    // since the CLOSED that created it. Every departure is here even when the
    // id is desired again now, because the loop cannot see the gap.
    for id in removed {
        retries.remove(&id);
    }

    for id in open.keys().cloned().collect::<Vec<_>>() {
        if desired.iter().any(|s| s.id == id) {
            continue;
        }
        if !budget.take() {
            return Pass::More;
        }
        if conn.send_raw(&json!(["CLOSE", id])).await.is_err() {
            return Pass::Failed;
        }
        open.remove(&id);
    }

    for sub in desired {
        // A filter change under the same id must reopen, not be skipped: the
        // relay replaces a subscription by id, so re-sending REQ is the update.
        if open.get(&sub.id) == Some(&sub.filter) {
            continue;
        }
        // Held back by a CLOSED: either waiting out its backoff, or terminal.
        // Both are `is_blocked`, which is what keeps a relay that rejects on
        // policy from being re-asked at the speed of the event loop.
        if retries.get(&sub.id).is_some_and(ClosedRetry::is_blocked) {
            continue;
        }
        if !budget.take() {
            return Pass::More;
        }
        if conn
            .send_raw(&json!(["REQ", sub.id, sub.filter]))
            .await
            .is_err()
        {
            return Pass::Failed;
        }
        open.insert(sub.id, sub.filter);
    }

    // Publishes go last, after subscriptions are current. A message sent
    // before its channel's REQ lands is fanned out to a subscription that does
    // not exist yet, so the author never sees their own message arrive.
    let mut outbox = session.outbox.lock().await;
    while let Some(outgoing) = outbox.pop() {
        if !budget.take() {
            outbox.push(outgoing);
            return Pass::More;
        }
        let id = outgoing.event.id.to_hex();
        if conn
            .send_raw(&json!(["EVENT", outgoing.event]))
            .await
            .is_err()
        {
            let _ = outgoing.reply.send(Err("send failed".into()));
            return Pass::Failed;
        }
        pending_ok.insert(id, outgoing.reply);
    }

    Pass::Done
}

/// Reopen schedule for one subscription the relay CLOSED.
#[derive(Default)]
struct ClosedRetry {
    /// When the reopen is due. `None` means "not waiting": either the delay has
    /// elapsed and reconcile may re-send, or `terminal` latched.
    due_at: Option<Instant>,
    /// Consecutive CLOSEDs, driving the exponential delay.
    attempts: u32,
    /// The relay rejected this filter for a reason retrying cannot change.
    terminal: bool,
}

impl ClosedRetry {
    /// True while reconcile must leave this subscription closed.
    fn is_blocked(&self) -> bool {
        self.terminal || self.due_at.is_some_and(|due| due > Instant::now())
    }

    /// Records a CLOSED and schedules the reopen its class calls for.
    fn schedule(&mut self, message: &str) {
        match classify_closed(message) {
            // Auth, access, or filter errors fail identically until something
            // outside this socket changes, so stop asking. Scoped to this
            // socket by construction: a reconnect retries once through the
            // normal path, which is deliberate — relay policy and our own auth
            // can change across a reconnect, and one REQ per reconnect is
            // bounded.
            ClosedClass::Terminal => {
                self.terminal = true;
                self.due_at = None;
            }
            ClosedClass::RateLimited => {
                let hinted = parse_retry_in_seconds(message)
                    .map(Duration::from_secs)
                    .unwrap_or(CLOSED_RATE_LIMIT_DEFAULT);
                // The longer of the two: a short hint must not undercut a
                // backoff already grown by repeated rejections.
                self.due_at = Some(Instant::now() + self.backoff().max(hinted));
                self.attempts = self.attempts.saturating_add(1);
            }
            ClosedClass::Retryable => {
                self.due_at = Some(Instant::now() + self.backoff());
                self.attempts = self.attempts.saturating_add(1);
            }
        }
    }

    /// Exponential delay for the current attempt, capped. The shift is bounded
    /// before it is taken, so a long-lived rejection cannot overflow its way
    /// back down to a short delay.
    fn backoff(&self) -> Duration {
        CLOSED_RETRY_BASE_DELAY
            .saturating_mul(1_u32 << self.attempts.min(16))
            .min(CLOSED_RETRY_MAX_DELAY)
    }
}

/// How a CLOSED message should be handled. The prefixes are the relay's own
/// machine-readable NIP-01 classes and must stay in step with
/// `relayClosedPolicy.ts` and `native_relay_client.rs`.
#[derive(Debug, PartialEq, Eq)]
enum ClosedClass {
    Retryable,
    RateLimited,
    Terminal,
}

fn classify_closed(message: &str) -> ClosedClass {
    let normalized = message.trim().to_ascii_lowercase();
    if normalized.starts_with("rate-limited:") {
        return ClosedClass::RateLimited;
    }
    // `auth-required:` is deliberately absent, i.e. retryable: it occurs
    // transiently when a REQ races the AUTH handshake after a reconnect, and
    // the backoff reopen re-sends once authenticated. A session that is
    // genuinely unauthenticated fails at `connect_authenticated` instead, so
    // this cannot loop forever.
    if [
        "restricted:",
        "blocked:",
        "invalid:",
        "pow:",
        "duplicate:",
        "unsupported:",
        "error: mixed search",
        "error: too many subscriptions",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix))
    {
        return ClosedClass::Terminal;
    }
    ClosedClass::Retryable
}

/// Parses the relay's canonical `retry in Ns` hint.
fn parse_retry_in_seconds(message: &str) -> Option<u64> {
    let after = &message[message.find("retry in ")? + "retry in ".len()..];
    after
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// A lapsed read is an idle relay, not a failure. Distinguished by variant
/// rather than by message text so a reworded error cannot turn every idle
/// period into a reconnect storm.
fn is_read_timeout(error: &WsClientError) -> bool {
    matches!(error, WsClientError::Timeout)
}

/// Normalizes a relay base URL to the WebSocket scheme the NIP-42 auth event
/// requires. `BUZZ_RELAY_URL` is documented as `http(s)://` for the CLI's REST
/// calls, so accepting both and converting is the least surprising behavior.
pub fn ws_url(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_is_its_own_class() {
        assert_eq!(
            classify_closed("rate-limited: quota exceeded; retry in 12s"),
            ClosedClass::RateLimited
        );
    }

    #[test]
    fn auth_required_stays_retryable() {
        // A REQ racing the AUTH handshake after reconnect must not latch
        // terminal, or the subscription is dead for the life of the socket.
        assert_eq!(
            classify_closed("auth-required: authenticate first"),
            ClosedClass::Retryable
        );
    }

    #[test]
    fn policy_rejections_are_terminal() {
        for message in ["restricted: not a member", "invalid: bad filter", "pow: 20"] {
            assert_eq!(classify_closed(message), ClosedClass::Terminal, "{message}");
        }
    }

    #[test]
    fn retry_hint_is_parsed_from_the_canonical_shape() {
        assert_eq!(
            parse_retry_in_seconds("rate-limited: quota exceeded; retry in 12s"),
            Some(12)
        );
        assert_eq!(parse_retry_in_seconds("rate-limited: slow down"), None);
    }

    #[test]
    fn backoff_is_capped_not_wrapped() {
        let mut retry = ClosedRetry {
            attempts: 30,
            ..Default::default()
        };
        assert_eq!(retry.backoff(), CLOSED_RETRY_MAX_DELAY);
        retry.attempts = 0;
        assert_eq!(retry.backoff(), CLOSED_RETRY_BASE_DELAY);
    }

    #[test]
    fn a_changed_filter_under_a_reused_id_records_a_departure() {
        let mut state = SessionState::default();
        state.replace_desired(vec![Subscription {
            id: "a".into(),
            filter: json!({"kinds": [9]}),
        }]);
        state.removed.clear();
        state.replace_desired(vec![Subscription {
            id: "a".into(),
            filter: json!({"kinds": [7]}),
        }]);
        assert!(
            state.removed.contains("a"),
            "a retired filter must not bequeath its backoff to the replacement"
        );
    }

    #[test]
    fn an_unchanged_subscription_is_not_a_departure() {
        let mut state = SessionState::default();
        let sub = Subscription {
            id: "a".into(),
            filter: json!({"kinds": [9]}),
        };
        state.replace_desired(vec![sub.clone()]);
        state.removed.clear();
        state.replace_desired(vec![sub]);
        assert!(state.removed.is_empty());
    }

    #[test]
    fn a_burst_fits_inside_the_relay_window() {
        // The relay admits 50 frames per 5 seconds. Startup legitimately needs
        // a dozen at once, so the bucket must not force them into single file.
        // A full burst must fit inside one relay window.
        const { assert!(FRAME_BURST < 50.0) };
        let mut budget = FrameBudget::new();
        for frame in 0..14 {
            assert!(budget.take(), "startup frame {frame} was throttled");
        }
    }

    #[test]
    fn sustained_writing_stays_under_the_relay_rate() {
        // Once the bucket is dry the refill sets the long-run rate, and that
        // is what a client writing forever actually spends.
        const { assert!(FRAME_REFILL_PER_SEC < 10.0) };
    }

    #[test]
    fn an_exhausted_budget_schedules_rather_than_spins() {
        let mut budget = FrameBudget::new();
        while budget.take() {}
        assert!(
            budget.next_available() > Instant::now(),
            "a dry bucket must hand back a future instant, or the loop spins"
        );
    }

    #[test]
    fn relay_urls_normalize_to_the_websocket_scheme() {
        assert_eq!(
            ws_url("https://relay.example.com/"),
            "wss://relay.example.com"
        );
        assert_eq!(ws_url("http://localhost:3000"), "ws://localhost:3000");
        assert_eq!(ws_url("wss://relay.example.com"), "wss://relay.example.com");
    }
}
