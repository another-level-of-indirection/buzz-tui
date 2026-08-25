/**
 * Plugin host — discovers, loads, and manages plugin lifecycles.
 *
 * Constructs the sandboxed BuzzPluginAPI for each plugin and tracks
 * registered panes, commands, and cleanup functions.
 */

import type { ReactElement } from "react";
import {
  discoverPlugins,
  loadPlugin,
  type BuzzPluginAPI,
  type BuzzReadAPI,
  type BuzzActionAPI,
  type PluginUI,
  type LoadedPlugin,
  type PluginManifest,
} from "../../plugin-sdk/index.ts";
import type { SessionDaemon } from "./daemon.ts";
import { join } from "path";

export interface PluginPane {
  pluginName: string;
  paneId: string;
  title: string;
  element: ReactElement;
}

export interface PluginCommand {
  pluginName: string;
  name: string;
  description: string;
  handler: (args: string[]) => void | Promise<void>;
}

export class PluginHost {
  private plugins: LoadedPlugin[] = [];
  private _panes: Map<string, PluginPane> = new Map();
  private _commands: Map<string, PluginCommand> = new Map();
  private _onUpdate: (() => void) | null = null;

  private daemon: SessionDaemon;
  private pubkey: string;
  private community: string;
  private focusedChannel: string | null;
  private noticeFn: (msg: string) => void;

  constructor(
    daemon: SessionDaemon,
    pubkey: string,
    community: string,
    noticeFn: (msg: string) => void
  ) {
    this.daemon = daemon;
    this.pubkey = pubkey;
    this.community = community;
    this.focusedChannel = null;
    this.noticeFn = noticeFn;
  }

  setFocusedChannel(channelId: string | null) {
    this.focusedChannel = channelId;
  }

  onUpdate(fn: () => void) {
    this._onUpdate = fn;
  }

  get panes(): PluginPane[] {
    return Array.from(this._panes.values());
  }

  get commands(): Map<string, PluginCommand> {
    return this._commands;
  }

  async loadAll(pluginsDir: string): Promise<void> {
    const dirs = await discoverPlugins(pluginsDir);
    for (const dir of dirs) {
      try {
        await this.loadOne(dir);
      } catch (e) {
        this.noticeFn(`Plugin load error (${dir}): ${e}`);
      }
    }
  }

  private async loadOne(dir: string): Promise<void> {
    const { manifest, factory } = await loadPlugin(dir);
    const instance = factory();

    const api = this.buildAPI(manifest);
    const cleanup =
      instance.activate(api) ?? undefined;

    this.plugins.push({
      manifest,
      instance,
      cleanup: typeof cleanup === "function" ? cleanup : undefined,
    });
  }

  private buildAPI(manifest: PluginManifest): BuzzPluginAPI {
    const daemon = this.daemon;
    const host = this;

    const buzz: BuzzReadAPI = {
      channelList: () => daemon.channelList(),
      storeSnapshot: (ch) => daemon.storeSnapshot(ch) as any,
      storeThread: (ch, root) => daemon.storeThread(ch, root) as any,
      canvasGet: (ch) => daemon.canvasGet(ch) as any,
      channelSearch: (q) =>
        daemon.channelSearch(q).then((r) => r.results as any),
    };

    const actions: BuzzActionAPI = {
      messageSend: (ch, content) =>
        daemon.messageSend(ch, content).then(() => {}),
      messageReply: (ch, content, replyTo) =>
        daemon.messageReply(ch, content, replyTo).then(() => {}),
      messageReact: (ch, target, emoji) =>
        daemon.messageReact(ch, target, emoji).then(() => {}),
    };

    const ui: PluginUI = {
      showNotice: (msg) => host.noticeFn(msg),
      setPane: (paneId, element) => {
        const fullId = `${manifest.name}:${paneId}`;
        if (element) {
          const decl = manifest.panes?.find((p) => p.id === paneId);
          host._panes.set(fullId, {
            pluginName: manifest.name,
            paneId: fullId,
            title: decl?.title ?? paneId,
            element,
          });
        } else {
          host._panes.delete(fullId);
        }
        host._onUpdate?.();
      },
      registerCommand: (name, handler) => {
        host._commands.set(name, {
          pluginName: manifest.name,
          name,
          description:
            manifest.commands?.find((c) => c.name === name)?.description ??
            `(from ${manifest.name})`,
          handler,
        });
      },
    };

    return {
      pubkey: host.pubkey,
      community: host.community,
      get focusedChannel() {
        return host.focusedChannel;
      },
      buzz,
      actions,
      ui,
    };
  }

  shutdown(): void {
    for (const plugin of this.plugins) {
      try {
        plugin.cleanup?.();
      } catch {
        /* ignore cleanup errors */
      }
    }
    this.plugins = [];
    this._panes.clear();
    this._commands.clear();
  }
}
