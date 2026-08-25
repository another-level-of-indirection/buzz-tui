/**
 * buzz-tui protocol — shared types between buzz-sessiond (Rust) and the
 * TypeScript shell.
 *
 * JSON-RPC 2.0 over stdio. Daemon pushes notifications; shell sends requests.
 */

import { z } from "zod/v4";

// ── JSON-RPC envelope ───────────────────────────────────────────────────────

export const RpcRequest = z.object({
  jsonrpc: z.literal("2.0"),
  method: z.string(),
  params: z.unknown().optional(),
  id: z.union([z.string(), z.number(), z.null()]),
});
export type RpcRequest = z.infer<typeof RpcRequest>;

export const RpcError = z.object({
  code: z.number(),
  message: z.string(),
});
export type RpcError = z.infer<typeof RpcError>;

export const RpcResponse = z.object({
  jsonrpc: z.literal("2.0"),
  result: z.unknown().optional(),
  error: RpcError.optional(),
  id: z.union([z.string(), z.number(), z.null()]),
});
export type RpcResponse = z.infer<typeof RpcResponse>;

export const RpcNotification = z.object({
  jsonrpc: z.literal("2.0"),
  method: z.string(),
  params: z.unknown(),
});
export type RpcNotification = z.infer<typeof RpcNotification>;

// ── Domain types ────────────────────────────────────────────────────────────

export const Community = z.object({
  url: z.string(),
  name: z.string(),
});
export type Community = z.infer<typeof Community>;

export const ChannelKind = z.enum(["Stream", "Forum", "Dm", "Other"]);
export type ChannelKind = z.infer<typeof ChannelKind>;

export const Channel = z.object({
  id: z.string(),
  name: z.string(),
  topic: z.string(),
  kind: ChannelKind,
  archived: z.boolean(),
  unread: z.number(),
  mentions: z.boolean(),
});
export type Channel = z.infer<typeof Channel>;

export const ReactionGroup = z.object({
  emoji: z.string(),
  count: z.number(),
  mine: z.string().nullable(),
});
export type ReactionGroup = z.infer<typeof ReactionGroup>;

export const Message = z.object({
  id: z.string(),
  author: z.string(),
  author_name: z.string(),
  created_at: z.number(),
  content: z.string(),
  edited: z.boolean(),
  reply_count: z.number(),
  reactions: z.array(ReactionGroup),
});
export type Message = z.infer<typeof Message>;

export const TypingUser = z.object({
  pubkey: z.string(),
  name: z.string(),
});
export type TypingUser = z.infer<typeof TypingUser>;

export const Canvas = z.object({
  id: z.string(),
  author: z.string(),
  updated_at: z.number(),
  content: z.string(),
});
export type Canvas = z.infer<typeof Canvas>;

// ── Notification params ─────────────────────────────────────────────────────

export const SessionReady = z.object({
  pubkey: z.string(),
  communities: z.array(Community),
});
export type SessionReady = z.infer<typeof SessionReady>;

export const SessionConnected = z.object({
  community: z.string(),
});
export type SessionConnected = z.infer<typeof SessionConnected>;

export const SessionDisconnected = z.object({
  community: z.string(),
  reason: z.string(),
});
export type SessionDisconnected = z.infer<typeof SessionDisconnected>;

export const StoreEvent = z.object({
  community: z.string(),
  subscription: z.string(),
  kind: z.number(),
  id: z.string(),
  pubkey: z.string(),
  author_name: z.string(),
  created_at: z.number(),
  content: z.string(),
});
export type StoreEvent = z.infer<typeof StoreEvent>;

export const StoreEose = z.object({
  community: z.string(),
  subscription: z.string(),
});
export type StoreEose = z.infer<typeof StoreEose>;

export const StoreChannelsLoaded = z.object({
  community: z.string(),
  count: z.number(),
});
export type StoreChannelsLoaded = z.infer<typeof StoreChannelsLoaded>;

// ── Request params ──────────────────────────────────────────────────────────

export const ChannelListParams = z.object({
  community: z.string().optional(),
});

export const ChannelFocusParams = z.object({
  channel: z.string(),
  community: z.string().optional(),
});

export const ChannelHistoryParams = z.object({
  channel: z.string(),
  community: z.string().optional(),
  before: z.number().optional(),
});

export const MessageSendParams = z.object({
  channel: z.string(),
  content: z.string(),
  community: z.string().optional(),
});

export const MessageReplyParams = z.object({
  channel: z.string(),
  content: z.string(),
  reply_to: z.string(),
  community: z.string().optional(),
});

export const MessageReactParams = z.object({
  channel: z.string(),
  target: z.string(),
  emoji: z.string().optional(),
  community: z.string().optional(),
});

export const TypingSetParams = z.object({
  channel: z.string(),
  community: z.string().optional(),
});

export const CanvasGetParams = z.object({
  channel: z.string(),
  community: z.string().optional(),
});

export const StoreSnapshotParams = z.object({
  channel: z.string(),
  community: z.string().optional(),
});

export const StoreSnapshot = z.object({
  channel: z.string(),
  messages: z.array(Message),
  typing: z.array(TypingUser),
});
export type StoreSnapshot = z.infer<typeof StoreSnapshot>;

// ── Thread ──────────────────────────────────────────────────────────────────

export const ThreadSnapshot = z.object({
  channel: z.string(),
  root: z.string(),
  messages: z.array(Message),
});
export type ThreadSnapshot = z.infer<typeof ThreadSnapshot>;

export const StoreThreadParams = z.object({
  channel: z.string(),
  root: z.string(),
  community: z.string().optional(),
});

// ── Search ──────────────────────────────────────────────────────────────────

export const SearchResult = z.object({
  id: z.string(),
  author: z.string(),
  author_name: z.string(),
  created_at: z.number(),
  content: z.string(),
});
export type SearchResult = z.infer<typeof SearchResult>;

export const ChannelSearchParams = z.object({
  query: z.string(),
  community: z.string().optional(),
});

export const ChannelSearchResult = z.object({
  results: z.array(SearchResult),
});
export type ChannelSearchResult = z.infer<typeof ChannelSearchResult>;

// ── Canvas set ──────────────────────────────────────────────────────────────

export const CanvasSetParams = z.object({
  channel: z.string(),
  content: z.string(),
  base_revision: z.string().optional(),
  community: z.string().optional(),
});

// ── Message delete ──────────────────────────────────────────────────────────

export const MessageDeleteParams = z.object({
  channel: z.string(),
  target: z.string(),
  community: z.string().optional(),
});

// ── Members ─────────────────────────────────────────────────────────────────

export const Member = z.object({
  pubkey: z.string(),
  name: z.string(),
  is_me: z.boolean(),
});
export type Member = z.infer<typeof Member>;

export const StoreMembersParams = z.object({
  channel: z.string(),
  community: z.string().optional(),
});

export const StoreMembersResult = z.object({
  channel: z.string(),
  members: z.array(Member),
});
export type StoreMembersResult = z.infer<typeof StoreMembersResult>;
