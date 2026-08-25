//! `buzz-sessiond` — headless Buzz session daemon.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout. The TypeScript shell launches this
//! process and owns the terminal; this process owns the relay connection,
//! store, identity, and read state.
//!
//! Private key material never leaves this process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use nostr::Keys;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use uuid::Uuid;

use buzz_session::config;
use buzz_session::identity;
use buzz_session::readstate::ReadState;
use buzz_session::session::{RelaySession, SessionEvent, Subscription};
use buzz_session::store::Store;

const DEFAULT_RELAY: &str = "http://localhost:3000";
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(30);
const READ_STATE_FLUSH: Duration = Duration::from_secs(15);
const PRESENCE_INTERVAL: Duration = Duration::from_secs(60);
const PRESENCE_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Value,
    id: Value,
}

#[derive(Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
    id: Value,
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

#[derive(Serialize)]
struct RpcNotification {
    jsonrpc: &'static str,
    method: String,
    params: Value,
}

impl RpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            result: Some(result),
            error: None,
            id,
        }
    }

    fn err(id: Value, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0",
            result: None,
            error: Some(RpcError { code, message }),
            id,
        }
    }
}

struct Daemon {
    keys: Keys,
    sessions: Vec<(String, Arc<RelaySession>)>,
    stores: HashMap<String, Store>,
    read_states: HashMap<String, ReadState>,
    out_tx: mpsc::Sender<String>,
}

impl Daemon {
    fn store(&self, url: &str) -> &Store {
        static EMPTY: std::sync::OnceLock<Store> = std::sync::OnceLock::new();
        self.stores
            .get(url)
            .unwrap_or_else(|| EMPTY.get_or_init(Store::default))
    }

    fn store_mut(&mut self, url: &str) -> &mut Store {
        self.stores.entry(url.to_string()).or_default()
    }

    async fn emit(&self, method: &str, params: Value) {
        let notification = RpcNotification {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
        };
        if let Ok(line) = serde_json::to_string(&notification) {
            let _ = self.out_tx.send(line).await;
        }
    }

    async fn handle(&mut self, request: RpcRequest) -> RpcResponse {
        match request.method.as_str() {
            "identity.status" => self.identity_status(request.id),
            "community.list" => self.community_list(request.id),
            "channel.list" => self.channel_list(request.id, &request.params),
            "channel.focus" => self.channel_focus(request.id, &request.params).await,
            "channel.history" => self.channel_history(request.id, &request.params).await,
            "channel.search" => self.channel_search(request.id, &request.params).await,
            "message.send" => self.message_send(request.id, &request.params).await,
            "message.reply" => self.message_reply(request.id, &request.params).await,
            "message.react" => self.message_react(request.id, &request.params).await,
            "message.delete" => self.message_delete(request.id, &request.params).await,
            "typing.set" => self.typing_set(request.id, &request.params).await,
            "canvas.get" => self.canvas_get(request.id, &request.params),
            "canvas.set" => self.canvas_set(request.id, &request.params).await,
            "store.snapshot" => self.store_snapshot(request.id, &request.params),
            "store.thread" => self.store_thread(request.id, &request.params),
            "store.members" => self.store_members(request.id, &request.params),
            _ => RpcResponse::err(
                request.id,
                -32601,
                format!("unknown method: {}", request.method),
            ),
        }
    }

    fn identity_status(&self, id: Value) -> RpcResponse {
        RpcResponse::ok(
            id,
            json!({
                "pubkey": self.keys.public_key().to_hex(),
                "communities": self.sessions.iter().map(|(url, _)| url.clone()).collect::<Vec<_>>(),
            }),
        )
    }

    fn community_list(&self, id: Value) -> RpcResponse {
        let communities: Vec<Value> = self
            .sessions
            .iter()
            .map(|(url, _)| {
                json!({
                    "url": url,
                    "name": display_name(url),
                })
            })
            .collect();
        RpcResponse::ok(id, json!(communities))
    }

    fn channel_list(&self, id: Value, params: &Value) -> RpcResponse {
        let url = params
            .get("community")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                self.sessions
                    .first()
                    .map(|(url, _)| url.as_str())
                    .unwrap_or("")
            });

        let store = self.store(url);
        let channels: Vec<Value> = store
            .channels()
            .iter()
            .map(|ch| {
                let log = store.log_or_empty(&ch.id);
                let frontier = self
                    .read_states
                    .get(url)
                    .map(|rs| rs.frontier(&ch.id))
                    .unwrap_or(0);
                let (unread, mentions) = log.unread_after(frontier, &self.keys.public_key());
                json!({
                    "id": ch.id.to_string(),
                    "name": store.channel_label(ch, &self.keys.public_key()),
                    "topic": ch.topic,
                    "kind": format!("{:?}", ch.kind),
                    "archived": ch.archived,
                    "unread": unread,
                    "mentions": mentions,
                })
            })
            .collect();
        RpcResponse::ok(id, json!(channels))
    }

    async fn channel_focus(&mut self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let url = params
            .get("community")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                self.sessions
                    .first()
                    .map(|(u, _)| u.as_str())
                    .unwrap_or("")
            })
            .to_string();

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let kinds: Vec<u32> = vec![9, 11, 40003, 9005, 5, 7, 20002, 40100];
        let filter = json!({
            "kinds": kinds,
            "#h": [channel.to_string()],
        });
        let sub = Subscription {
            id: format!("live-{channel}"),
            filter,
        };
        session.set_subscriptions(vec![sub]).await;

        let history_filter = json!({
            "kinds": [9u32, 11, 40003, 9005, 5, 7],
            "#h": [channel.to_string()],
            "limit": 200,
        });
        match session.fetch(history_filter, FETCH_TIMEOUT).await {
            Ok(events) => {
                let count = events.len();
                {
                    let store = self.store_mut(&url);
                    for event in &events {
                        store.apply(event);
                    }
                }
                let newest = self.store(&url).log_or_empty(&channel).newest_at();
                if let (Some(rs), Some(at)) = (self.read_states.get_mut(&url), newest) {
                    rs.advance(&channel, at);
                }
                RpcResponse::ok(
                    id,
                    json!({
                        "channel": channel.to_string(),
                        "messages": count,
                    }),
                )
            }
            Err(e) => RpcResponse::err(id, -32000, e),
        }
    }

    async fn channel_history(&mut self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let url = self.resolve_community(params);
        let before = params.get("before").and_then(|v| v.as_u64());

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let mut filter = json!({
            "kinds": [9u32, 11, 40003, 9005, 5, 7],
            "#h": [channel.to_string()],
            "limit": 200,
        });
        if let Some(before) = before {
            filter["until"] = json!(before);
        }

        match session.fetch(filter, FETCH_TIMEOUT).await {
            Ok(events) => {
                let store = self.store_mut(&url);
                for event in &events {
                    store.apply(event);
                }
                RpcResponse::ok(id, json!({"loaded": events.len()}))
            }
            Err(e) => RpcResponse::err(id, -32000, e),
        }
    }

    async fn message_send(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let Some(content) = params.get("content").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing content".into());
        };
        let url = self.resolve_community(params);

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let store = self.store(&url);
        let mention_pubkeys = store.resolve_mentions(content);
        let mut tags: Vec<nostr::Tag> = vec![
            nostr::Tag::parse(["h", &channel.to_string()]).unwrap(),
        ];
        for hex in &mention_pubkeys {
            if let Ok(tag) = nostr::Tag::parse(["p", hex]) {
                tags.push(tag);
            }
        }

        let builder = nostr::EventBuilder::new(nostr::Kind::Custom(9), content).tags(tags);
        match builder.sign_with_keys(&self.keys) {
            Ok(event) => match session.publish(event, PUBLISH_TIMEOUT).await {
                Ok(()) => RpcResponse::ok(id, json!({"sent": true})),
                Err(e) => RpcResponse::err(id, -32000, e),
            },
            Err(e) => RpcResponse::err(id, -32000, e.to_string()),
        }
    }

    async fn message_reply(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let Some(content) = params.get("content").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing content".into());
        };
        let Some(reply_to) = params.get("reply_to").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing reply_to".into());
        };
        let Ok(reply_id) = nostr::EventId::from_hex(reply_to) else {
            return RpcResponse::err(id, -32602, "invalid reply_to event id".into());
        };
        let url = self.resolve_community(params);

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let store = self.store(&url);
        let mention_pubkeys = store.resolve_mentions(content);

        let root = store
            .log_or_empty(&channel)
            .messages()
            .iter()
            .find(|m| m.id == reply_id)
            .and_then(|m| m.root)
            .unwrap_or(reply_id);

        let mut tags: Vec<nostr::Tag> = vec![
            nostr::Tag::parse(["h", &channel.to_string()]).unwrap(),
            nostr::Tag::parse(["e", &root.to_hex(), "", "root"]).unwrap(),
        ];
        if root != reply_id {
            tags.push(nostr::Tag::parse(["e", &reply_id.to_hex(), "", "reply"]).unwrap());
        } else {
            tags.push(nostr::Tag::parse(["e", &reply_id.to_hex(), "", "reply"]).unwrap());
        }
        for hex in &mention_pubkeys {
            if let Ok(tag) = nostr::Tag::parse(["p", hex]) {
                tags.push(tag);
            }
        }

        let builder = nostr::EventBuilder::new(nostr::Kind::Custom(9), content).tags(tags);
        match builder.sign_with_keys(&self.keys) {
            Ok(event) => match session.publish(event, PUBLISH_TIMEOUT).await {
                Ok(()) => RpcResponse::ok(id, json!({"sent": true})),
                Err(e) => RpcResponse::err(id, -32000, e),
            },
            Err(e) => RpcResponse::err(id, -32000, e.to_string()),
        }
    }

    async fn message_react(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(target_str) = params.get("target").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing target".into());
        };
        let Ok(_target) = nostr::EventId::from_hex(target_str) else {
            return RpcResponse::err(id, -32602, "invalid target event id".into());
        };
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let emoji = params
            .get("emoji")
            .and_then(|v| v.as_str())
            .unwrap_or("+");
        let url = self.resolve_community(params);

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let tags = vec![
            nostr::Tag::parse(["h", channel_str]).unwrap(),
            nostr::Tag::parse(["e", target_str]).unwrap(),
        ];

        let builder = nostr::EventBuilder::new(nostr::Kind::Custom(7), emoji).tags(tags);
        match builder.sign_with_keys(&self.keys) {
            Ok(event) => match session.publish(event, PUBLISH_TIMEOUT).await {
                Ok(()) => RpcResponse::ok(id, json!({"sent": true})),
                Err(e) => RpcResponse::err(id, -32000, e),
            },
            Err(e) => RpcResponse::err(id, -32000, e.to_string()),
        }
    }

    async fn typing_set(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let url = self.resolve_community(params);

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let tags = vec![nostr::Tag::parse(["h", channel_str]).unwrap()];
        let builder = nostr::EventBuilder::new(nostr::Kind::Custom(20002), "").tags(tags);
        match builder.sign_with_keys(&self.keys) {
            Ok(event) => {
                let _ = session.publish(event, Duration::from_secs(5)).await;
                RpcResponse::ok(id, json!({"sent": true}))
            }
            Err(e) => RpcResponse::err(id, -32000, e.to_string()),
        }
    }

    fn canvas_get(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let url = self.resolve_community(params);
        let store = self.store(&url);

        match store.canvas(&channel) {
            Some(canvas) => RpcResponse::ok(
                id,
                json!({
                    "id": canvas.id.to_hex(),
                    "author": canvas.author.to_hex(),
                    "updated_at": canvas.updated_at,
                    "content": canvas.content,
                }),
            ),
            None => RpcResponse::ok(id, Value::Null),
        }
    }

    fn store_snapshot(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let url = self.resolve_community(params);
        let store = self.store(&url);
        let log = store.log_or_empty(&channel);

        let messages: Vec<Value> = log
            .messages()
            .iter()
            .filter(|m| m.root.is_none())
            .map(|m| {
                let reactions = log.reactions(m.id, &self.keys.public_key());
                let reply_counts = log.reply_counts();
                json!({
                    "id": m.id.to_hex(),
                    "author": m.author.to_hex(),
                    "author_name": store.display_name(&m.author),
                    "created_at": m.created_at,
                    "content": m.content,
                    "edited": m.edited,
                    "reply_count": reply_counts.get(&m.id).copied().unwrap_or(0),
                    "reactions": reactions.iter().map(|r| json!({
                        "emoji": r.emoji,
                        "count": r.count,
                        "mine": r.mine.map(|id| id.to_hex()),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let typing: Vec<Value> = log
            .typing(now, &self.keys.public_key())
            .iter()
            .map(|pk| {
                json!({
                    "pubkey": pk.to_hex(),
                    "name": store.display_name(pk),
                })
            })
            .collect();

        RpcResponse::ok(
            id,
            json!({
                "channel": channel.to_string(),
                "messages": messages,
                "typing": typing,
            }),
        )
    }

    fn store_thread(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let Some(root_str) = params.get("root").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing root".into());
        };
        let Ok(root) = nostr::EventId::from_hex(root_str) else {
            return RpcResponse::err(id, -32602, "invalid root event id".into());
        };
        let url = self.resolve_community(params);
        let store = self.store(&url);
        let log = store.log_or_empty(&channel);

        let thread: Vec<Value> = log
            .thread(root)
            .iter()
            .map(|m| {
                let reactions = log.reactions(m.id, &self.keys.public_key());
                json!({
                    "id": m.id.to_hex(),
                    "author": m.author.to_hex(),
                    "author_name": store.display_name(&m.author),
                    "created_at": m.created_at,
                    "content": m.content,
                    "edited": m.edited,
                    "reply_count": 0,
                    "reactions": reactions.iter().map(|r| json!({
                        "emoji": r.emoji,
                        "count": r.count,
                        "mine": r.mine.map(|id| id.to_hex()),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();

        RpcResponse::ok(
            id,
            json!({
                "channel": channel.to_string(),
                "root": root_str,
                "messages": thread,
            }),
        )
    }

    fn store_members(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let url = self.resolve_community(params);
        let store = self.store(&url);

        let ch = match store.channels().iter().find(|c| c.id == channel) {
            Some(c) => c,
            None => return RpcResponse::ok(id, json!({"members": []})),
        };

        let participants = store.participants_of(ch);
        let members: Vec<Value> = participants
            .iter()
            .map(|pk| {
                json!({
                    "pubkey": pk.to_hex(),
                    "name": store.display_name(pk),
                    "is_me": *pk == self.keys.public_key(),
                })
            })
            .collect();

        RpcResponse::ok(id, json!({
            "channel": channel.to_string(),
            "members": members,
        }))
    }

    async fn channel_search(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(query) = params.get("query").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing query".into());
        };
        if query.trim().is_empty() {
            return RpcResponse::err(id, -32602, "empty query".into());
        };
        let url = self.resolve_community(params);

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let filter = json!({
            "kinds": [9u32, 11],
            "search": query,
            "limit": 40,
        });

        match session.fetch(filter, FETCH_TIMEOUT).await {
            Ok(events) => {
                let store = self.store(&url);
                let results: Vec<Value> = events
                    .iter()
                    .map(|e| {
                        json!({
                            "id": e.id.to_hex(),
                            "author": e.pubkey.to_hex(),
                            "author_name": store.display_name(&e.pubkey),
                            "created_at": e.created_at.as_secs(),
                            "content": e.content,
                        })
                    })
                    .collect();
                RpcResponse::ok(id, json!({"results": results}))
            }
            Err(e) => RpcResponse::err(id, -32000, e),
        }
    }

    async fn canvas_set(&mut self, id: Value, params: &Value) -> RpcResponse {
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let Ok(channel) = Uuid::parse_str(channel_str) else {
            return RpcResponse::err(id, -32602, "invalid channel UUID".into());
        };
        let Some(content) = params.get("content").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing content".into());
        };
        let base_revision = params.get("base_revision").and_then(|v| v.as_str());
        let url = self.resolve_community(params);

        // Refuse-to-clobber: if the caller passes base_revision, verify it
        // matches the current head. A mismatch means someone else saved while
        // the editor was open.
        let store = self.store(&url);
        if let Some(expected) = base_revision {
            if let Some(canvas) = store.canvas(&channel) {
                if canvas.id.to_hex() != expected {
                    return RpcResponse::err(
                        id,
                        -32001,
                        format!(
                            "canvas was updated by {} while you were editing (revision {})",
                            store.display_name(&canvas.author),
                            &canvas.id.to_hex()[..8]
                        ),
                    );
                }
            }
        }

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let tags = vec![nostr::Tag::parse(["h", &channel.to_string()]).unwrap()];
        let builder =
            nostr::EventBuilder::new(nostr::Kind::Custom(40100), content).tags(tags);
        match builder.sign_with_keys(&self.keys) {
            Ok(event) => match session.publish(event, PUBLISH_TIMEOUT).await {
                Ok(()) => RpcResponse::ok(id, json!({"saved": true})),
                Err(e) => RpcResponse::err(id, -32000, e),
            },
            Err(e) => RpcResponse::err(id, -32000, e.to_string()),
        }
    }

    async fn message_delete(&self, id: Value, params: &Value) -> RpcResponse {
        let Some(target_str) = params.get("target").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing target".into());
        };
        let Some(channel_str) = params.get("channel").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, -32602, "missing channel".into());
        };
        let url = self.resolve_community(params);

        let Some((_, session)) = self.sessions.iter().find(|(u, _)| *u == url) else {
            return RpcResponse::err(id, -32602, "unknown community".into());
        };

        let tags = vec![
            nostr::Tag::parse(["h", channel_str]).unwrap(),
            nostr::Tag::parse(["e", target_str]).unwrap(),
        ];
        let builder = nostr::EventBuilder::new(nostr::Kind::Custom(5), "").tags(tags);
        match builder.sign_with_keys(&self.keys) {
            Ok(event) => match session.publish(event, PUBLISH_TIMEOUT).await {
                Ok(()) => RpcResponse::ok(id, json!({"deleted": true})),
                Err(e) => RpcResponse::err(id, -32000, e),
            },
            Err(e) => RpcResponse::err(id, -32000, e.to_string()),
        }
    }

    fn resolve_community(&self, params: &Value) -> String {
        params
            .get("community")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                self.sessions
                    .first()
                    .map(|(url, _)| url.clone())
                    .unwrap_or_default()
            })
    }
}

fn relay_urls() -> Vec<String> {
    let env = std::env::var("BUZZ_RELAY_URL").ok();
    let urls = config::communities(env.as_deref());
    if urls.is_empty() {
        vec![DEFAULT_RELAY.to_string()]
    } else {
        urls
    }
}

fn ws_url(base: &str) -> String {
    buzz_session::session::ws_url(base)
}

fn display_name(url: &str) -> String {
    config::name_for(url).unwrap_or_else(|| {
        let ws = ws_url(url);
        let host = ws
            .split("://")
            .nth(1)
            .unwrap_or(&ws)
            .split('/')
            .next()
            .unwrap_or(&ws);
        let first = host.split('.').next().unwrap_or(host);
        if first.is_empty() || first.chars().all(|c| c.is_ascii_digit() || c == ':') {
            host.to_string()
        } else {
            first.to_string()
        }
    })
}

fn load_auth_tag(keys: &Keys) -> Result<Option<nostr::Tag>> {
    let Ok(raw) = std::env::var("BUZZ_AUTH_TAG") else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    buzz_sdk::nip_oa::verify_auth_tag(&raw, &keys.public_key())
        .map_err(|e| anyhow::anyhow!("BUZZ_AUTH_TAG does not attest this key: {e}"))?;
    let tag = buzz_sdk::nip_oa::parse_auth_tag(&raw)
        .map_err(|e| anyhow::anyhow!("BUZZ_AUTH_TAG is not a valid NIP-OA tag: {e}"))?;
    Ok(Some(tag))
}

fn spawn_presence_heartbeat(session: Arc<RelaySession>, keys: Keys) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PRESENCE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Ok(builder) = buzz_sdk::build_presence_update("online") else {
                return;
            };
            let Ok(event) = builder.sign_with_keys(&keys) else {
                return;
            };
            let _ = session.publish(event, PRESENCE_PUBLISH_TIMEOUT).await;
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (keys, _key_source) = identity::load()?;
    let auth_tag = load_auth_tag(&keys)?;
    let relays = relay_urls();

    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (event_tx, mut session_rx) = mpsc::channel::<(String, SessionEvent)>(512);

    let mut sessions = Vec::new();
    let mut stores = HashMap::new();
    let mut read_states = HashMap::new();

    for relay in &relays {
        let ws = ws_url(relay);
        let (session, mut events) =
            RelaySession::start(ws.clone(), keys.clone(), auth_tag.clone());
        sessions.push((ws.clone(), Arc::clone(&session)));
        stores.insert(ws.clone(), Store::default());
        read_states.insert(ws.clone(), ReadState::load());

        let forward = event_tx.clone();
        let tag = ws.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if forward.send((tag.clone(), event)).await.is_err() {
                    return;
                }
            }
        });

        spawn_presence_heartbeat(Arc::clone(&session), keys.clone());
    }
    drop(event_tx);

    let mut daemon = Daemon {
        keys: keys.clone(),
        sessions,
        stores,
        read_states,
        out_tx: out_tx.clone(),
    };

    // Emit ready notification
    daemon.emit("session.ready", json!({
        "pubkey": keys.public_key().to_hex(),
        "communities": relays.iter().map(|r| json!({"url": r, "name": display_name(r)})).collect::<Vec<_>>(),
    })).await;

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let stdout = tokio::io::stdout();

    // Writer task: serializes all output through stdout
    let writer = tokio::spawn(async move {
        let mut out = stdout;
        while let Some(line) = out_rx.recv().await {
            let _ = out.write_all(line.as_bytes()).await;
            let _ = out.write_all(b"\n").await;
            let _ = out.flush().await;
        }
    });

    let mut flush = tokio::time::interval(READ_STATE_FLUSH);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut line = String::new();
    loop {
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            match serde_json::from_str::<RpcRequest>(trimmed) {
                                Ok(request) => {
                                    if request.jsonrpc != "2.0" {
                                        let resp = RpcResponse::err(
                                            request.id,
                                            -32600,
                                            "jsonrpc must be \"2.0\"".into(),
                                        );
                                        if let Ok(out) = serde_json::to_string(&resp) {
                                            let _ = out_tx.send(out).await;
                                        }
                                    } else {
                                        let resp = daemon.handle(request).await;
                                        if let Ok(out) = serde_json::to_string(&resp) {
                                            let _ = out_tx.send(out).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    let resp = RpcResponse::err(
                                        Value::Null,
                                        -32700,
                                        format!("parse error: {e}"),
                                    );
                                    if let Ok(out) = serde_json::to_string(&resp) {
                                        let _ = out_tx.send(out).await;
                                    }
                                }
                            }
                        }
                        line.clear();
                    }
                    Err(e) => {
                        eprintln!("stdin error: {e}");
                        break;
                    }
                }
            }
            event = session_rx.recv() => {
                match event {
                    Some((url, session_event)) => {
                        match session_event {
                            SessionEvent::Connected => {
                                daemon.emit("session.connected", json!({"community": url})).await;
                                let session = daemon.sessions.iter()
                                    .find(|(u, _)| *u == url)
                                    .map(|(_, s)| Arc::clone(s));
                                if let Some(session) = session {
                                    // Fetch channel metadata + member rosters
                                    let filter = json!({
                                        "kinds": [39000u32, 39002],
                                        "#p": [keys.public_key().to_hex()],
                                        "limit": 500,
                                    });
                                    if let Ok(events) = session.fetch(filter, FETCH_TIMEOUT).await {
                                        let store = daemon.store_mut(&url);
                                        for event in &events {
                                            store.apply(event);
                                        }
                                        daemon.emit("store.channels_loaded", json!({
                                            "community": url,
                                            "count": events.len(),
                                        })).await;
                                    }
                                    // Fetch profiles for all participants
                                    let authors = daemon.store(&url).all_participants();
                                    if !authors.is_empty() {
                                        let profile_filter = json!({
                                            "kinds": [0u32],
                                            "authors": authors,
                                            "limit": 500,
                                        });
                                        if let Ok(profiles) = session.fetch(profile_filter, FETCH_TIMEOUT).await {
                                            let store = daemon.store_mut(&url);
                                            for event in &profiles {
                                                store.apply(event);
                                            }
                                        }
                                    }
                                    // Fetch read state
                                    let rs_filter = json!({
                                        "kinds": [30078u32],
                                        "authors": [keys.public_key().to_hex()],
                                        "#t": ["read-state"],
                                        "limit": 50,
                                    });
                                    if let Ok(rs_events) = session.fetch(rs_filter, FETCH_TIMEOUT).await {
                                        if let Some(rs) = daemon.read_states.get_mut(&url) {
                                            for event in &rs_events {
                                                let conflict = rs.merge(event, &keys);
                                                if conflict {
                                                    rs.rotate_slot();
                                                }
                                            }
                                        }
                                    }
                                    // Fetch overview for unread counts
                                    let channel_ids: Vec<String> = daemon.store(&url).channels()
                                        .iter()
                                        .take(128)
                                        .map(|ch| ch.id.to_string())
                                        .collect();
                                    if !channel_ids.is_empty() {
                                        let overview_filter = json!({
                                            "kinds": [9u32, 11],
                                            "#h": channel_ids,
                                            "limit": 500,
                                        });
                                        if let Ok(overview) = session.fetch(overview_filter, FETCH_TIMEOUT).await {
                                            let store = daemon.store_mut(&url);
                                            for event in &overview {
                                                store.apply(event);
                                            }
                                        }
                                    }
                                }
                            }
                            SessionEvent::Disconnected(reason) => {
                                daemon.emit("session.disconnected", json!({"community": url, "reason": reason})).await;
                            }
                            SessionEvent::Event { subscription_id, event } => {
                                let store = daemon.store_mut(&url);
                                store.apply(&event);
                                daemon.emit("store.event", json!({
                                    "community": url,
                                    "subscription": subscription_id,
                                    "kind": u32::from(event.kind.as_u16()),
                                    "id": event.id.to_hex(),
                                    "pubkey": event.pubkey.to_hex(),
                                    "author_name": daemon.store(&url).display_name(&event.pubkey),
                                    "created_at": event.created_at.as_secs(),
                                    "content": event.content,
                                })).await;
                            }
                            SessionEvent::Eose { subscription_id } => {
                                daemon.emit("store.eose", json!({
                                    "community": url,
                                    "subscription": subscription_id,
                                })).await;
                            }
                            SessionEvent::Notice(message) => {
                                daemon.emit("session.notice", json!({
                                    "community": url,
                                    "message": message,
                                })).await;
                            }
                        }
                    }
                    None => break,
                }
            }
            _ = flush.tick() => {
                for (url, rs) in &mut daemon.read_states {
                    if rs.is_dirty() {
                        if let Ok(builder) = rs.build(&keys) {
                            if let Ok(event) = builder.sign_with_keys(&keys) {
                                if let Some((_, session)) = daemon.sessions.iter().find(|(u, _)| u == url) {
                                    let _ = session.publish(event, Duration::from_secs(5)).await;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    for (_, session) in &daemon.sessions {
        session.shutdown();
    }
    drop(out_tx);
    let _ = writer.await;

    Ok(())
}
