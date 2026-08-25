/**
 * @buzz-tui/plugin-sdk — API surface for third-party panes and commands.
 *
 * Plugins run in the same Bun process as the shell. They have no access to
 * private keys or raw Nostr events — only the scoped API this module exposes.
 *
 * A plugin is a directory with a manifest.json and an entry module that
 * default-exports a PluginFactory.
 */

import type { ReactElement } from "react";

// ── Manifest ────────────────────────────────────────────────────────────────

export interface PluginManifest {
  name: string;
  version: string;
  description?: string;
  entry: string;
  panes?: PaneDeclaration[];
  commands?: CommandDeclaration[];
  keybindings?: KeybindingDeclaration[];
}

export interface PaneDeclaration {
  id: string;
  title: string;
  /** Default position: "right" | "bottom" | "overlay" */
  position?: "right" | "bottom" | "overlay";
}

export interface CommandDeclaration {
  name: string;
  description: string;
}

export interface KeybindingDeclaration {
  key: string;
  ctrl?: boolean;
  shift?: boolean;
  meta?: boolean;
  action: string;
}

// ── Plugin API (passed to activate) ─────────────────────────────────────────

export interface BuzzPluginAPI {
  /** Current user's pubkey (hex). No secret material. */
  readonly pubkey: string;

  /** Active community URL. */
  readonly community: string;

  /** Focused channel ID, or null. */
  readonly focusedChannel: string | null;

  /** Read-only Buzz data access. */
  readonly buzz: BuzzReadAPI;

  /** Scoped write access — only what the host explicitly allows. */
  readonly actions: BuzzActionAPI;

  /** UI integration. */
  readonly ui: PluginUI;
}

export interface BuzzReadAPI {
  channelList(): Promise<ChannelInfo[]>;
  storeSnapshot(channel: string): Promise<SnapshotData>;
  storeThread(channel: string, root: string): Promise<ThreadData>;
  canvasGet(channel: string): Promise<CanvasData | null>;
  channelSearch(query: string): Promise<SearchResultData[]>;
}

export interface BuzzActionAPI {
  messageSend(channel: string, content: string): Promise<void>;
  messageReply(channel: string, content: string, replyTo: string): Promise<void>;
  messageReact(channel: string, target: string, emoji?: string): Promise<void>;
}

export interface PluginUI {
  /** Show a transient notice in the status area. */
  showNotice(message: string): void;
  /** Register a React element to render in a pane slot. */
  setPane(paneId: string, element: ReactElement | null): void;
  /** Register a slash command handler. */
  registerCommand(
    name: string,
    handler: (args: string[]) => void | Promise<void>
  ): void;
}

// ── Data shapes (mirrors protocol but decoupled for stability) ──────────────

export interface ChannelInfo {
  id: string;
  name: string;
  topic: string;
  kind: string;
  unread: number;
  mentions: boolean;
}

export interface MessageData {
  id: string;
  author: string;
  author_name: string;
  created_at: number;
  content: string;
  edited: boolean;
  reply_count: number;
  reactions: { emoji: string; count: number; mine: string | null }[];
}

export interface SnapshotData {
  channel: string;
  messages: MessageData[];
  typing: { pubkey: string; name: string }[];
}

export interface ThreadData {
  channel: string;
  root: string;
  messages: MessageData[];
}

export interface CanvasData {
  id: string;
  author: string;
  updated_at: number;
  content: string;
}

export interface SearchResultData {
  id: string;
  author: string;
  author_name: string;
  created_at: number;
  content: string;
}

// ── Plugin lifecycle ────────────────────────────────────────────────────────

export interface PluginInstance {
  /** Called when the plugin is loaded. Return a cleanup function if needed. */
  activate(api: BuzzPluginAPI): void | (() => void);
}

/** A plugin module default-exports a factory function. */
export type PluginFactory = () => PluginInstance;

// ── Plugin loader (used by the shell) ───────────────────────────────────────

export interface LoadedPlugin {
  manifest: PluginManifest;
  instance: PluginInstance;
  cleanup?: () => void;
}

export async function loadPlugin(
  pluginDir: string
): Promise<{ manifest: PluginManifest; factory: PluginFactory }> {
  const { join, resolve } = await import("path");
  const { readFileSync } = await import("fs");

  const manifestPath = join(pluginDir, "manifest.json");
  const raw = readFileSync(manifestPath, "utf-8");
  const manifest = JSON.parse(raw) as PluginManifest;

  if (!manifest.name || !manifest.version || !manifest.entry) {
    throw new Error(
      `Invalid plugin manifest at ${manifestPath}: requires name, version, entry`
    );
  }

  const entryPath = resolve(pluginDir, manifest.entry);
  const mod = await import(entryPath);
  const factory = mod.default as PluginFactory;

  if (typeof factory !== "function") {
    throw new Error(
      `Plugin ${manifest.name}: entry must default-export a PluginFactory function`
    );
  }

  return { manifest, factory };
}

export async function discoverPlugins(
  pluginsDir: string
): Promise<string[]> {
  const { readdirSync, existsSync, statSync } = await import("fs");
  const { join } = await import("path");

  if (!existsSync(pluginsDir)) return [];

  return readdirSync(pluginsDir)
    .map((name) => join(pluginsDir, name))
    .filter((dir) => {
      try {
        return (
          statSync(dir).isDirectory() &&
          existsSync(join(dir, "manifest.json"))
        );
      } catch {
        return false;
      }
    });
}
