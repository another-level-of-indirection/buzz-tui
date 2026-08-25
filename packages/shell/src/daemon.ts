/**
 * Client for buzz-sessiond — spawns the daemon and speaks JSON-RPC over stdio.
 */

import { spawn, type Subprocess } from "bun";
import { EventEmitter } from "events";
import type {
  RpcResponse,
  RpcNotification,
  Channel,
  Community,
  StoreSnapshot,
  StoreEvent,
  Canvas,
  ThreadSnapshot,
  SearchResult,
  Member,
} from "../../protocol/index.ts";

export interface DaemonEvents {
  ready: [{ pubkey: string; communities: Community[] }];
  connected: [{ community: string }];
  disconnected: [{ community: string; reason: string }];
  event: [StoreEvent];
  eose: [{ community: string; subscription: string }];
  channels_loaded: [{ community: string; count: number }];
  notice: [{ community: string; message: string }];
  error: [Error];
}

export class SessionDaemon extends EventEmitter<DaemonEvents> {
  private proc: Subprocess | null = null;
  private nextId = 1;
  private pending = new Map<
    number,
    { resolve: (v: unknown) => void; reject: (e: Error) => void }
  >();
  private buffer = "";

  async start(daemonPath?: string): Promise<void> {
    const bin = daemonPath ?? "buzz-sessiond";
    this.proc = spawn([bin], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "inherit",
      env: { ...process.env },
    });

    this.readLoop();
  }

  private async readLoop(): Promise<void> {
    const stdout = this.proc?.stdout;
    if (!stdout || typeof stdout === "number") return;

    const decoder = new TextDecoder();
    const reader = (stdout as ReadableStream<Uint8Array>).getReader();

    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        this.buffer += decoder.decode(value, { stream: true });
        const lines = this.buffer.split("\n");
        this.buffer = lines.pop() ?? "";

        for (const line of lines) {
          if (!line.trim()) continue;
          try {
            const msg = JSON.parse(line);
            if ("id" in msg && msg.id !== undefined) {
              this.handleResponse(msg as RpcResponse);
            } else if ("method" in msg) {
              this.handleNotification(msg as RpcNotification);
            }
          } catch {
            // ignore malformed lines
          }
        }
      }
    } catch (err) {
      this.emit("error", err instanceof Error ? err : new Error(String(err)));
    }
  }

  private handleResponse(resp: RpcResponse): void {
    const id = typeof resp.id === "number" ? resp.id : parseInt(String(resp.id));
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);

    if (resp.error) {
      pending.reject(new Error(`${resp.error.code}: ${resp.error.message}`));
    } else {
      pending.resolve(resp.result);
    }
  }

  private handleNotification(notif: RpcNotification): void {
    const params = notif.params as Record<string, unknown>;
    switch (notif.method) {
      case "session.ready":
        this.emit("ready", params as DaemonEvents["ready"][0]);
        break;
      case "session.connected":
        this.emit("connected", params as DaemonEvents["connected"][0]);
        break;
      case "session.disconnected":
        this.emit("disconnected", params as DaemonEvents["disconnected"][0]);
        break;
      case "store.event":
        this.emit("event", params as StoreEvent);
        break;
      case "store.eose":
        this.emit("eose", params as DaemonEvents["eose"][0]);
        break;
      case "store.channels_loaded":
        this.emit("channels_loaded", params as DaemonEvents["channels_loaded"][0]);
        break;
      case "session.notice":
        this.emit("notice", params as DaemonEvents["notice"][0]);
        break;
    }
  }

  async call(method: string, params?: unknown): Promise<unknown> {
    if (!this.proc?.stdin) {
      throw new Error("daemon not started");
    }
    const id = this.nextId++;
    const request = JSON.stringify({
      jsonrpc: "2.0",
      method,
      params: params ?? {},
      id,
    });

    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      const stdin = this.proc!.stdin!;
      if (typeof stdin === "number") {
        reject(new Error("stdin is not writable"));
        return;
      }
      (stdin as { write(data: string): void }).write(request + "\n");
    });
  }

  // ── Typed convenience methods ───────────────────────────────────────────

  async identityStatus(): Promise<{ pubkey: string; communities: string[] }> {
    return (await this.call("identity.status")) as {
      pubkey: string;
      communities: string[];
    };
  }

  async communityList(): Promise<Community[]> {
    return (await this.call("community.list")) as Community[];
  }

  async channelList(community?: string): Promise<Channel[]> {
    return (await this.call("channel.list", { community })) as Channel[];
  }

  async channelFocus(
    channel: string,
    community?: string
  ): Promise<{ channel: string; messages: number }> {
    return (await this.call("channel.focus", {
      channel,
      community,
    })) as { channel: string; messages: number };
  }

  async channelHistory(
    channel: string,
    opts?: { community?: string; before?: number }
  ): Promise<{ loaded: number }> {
    return (await this.call("channel.history", {
      channel,
      ...opts,
    })) as { loaded: number };
  }

  async messageSend(
    channel: string,
    content: string,
    community?: string
  ): Promise<{ sent: boolean }> {
    return (await this.call("message.send", {
      channel,
      content,
      community,
    })) as { sent: boolean };
  }

  async messageReply(
    channel: string,
    content: string,
    replyTo: string,
    community?: string
  ): Promise<{ sent: boolean }> {
    return (await this.call("message.reply", {
      channel,
      content,
      reply_to: replyTo,
      community,
    })) as { sent: boolean };
  }

  async messageReact(
    channel: string,
    target: string,
    emoji?: string,
    community?: string
  ): Promise<{ sent: boolean }> {
    return (await this.call("message.react", {
      channel,
      target,
      emoji,
      community,
    })) as { sent: boolean };
  }

  async typingSet(
    channel: string,
    community?: string
  ): Promise<{ sent: boolean }> {
    return (await this.call("typing.set", {
      channel,
      community,
    })) as { sent: boolean };
  }

  async canvasGet(
    channel: string,
    community?: string
  ): Promise<Canvas | null> {
    return (await this.call("canvas.get", {
      channel,
      community,
    })) as Canvas | null;
  }

  async storeSnapshot(
    channel: string,
    community?: string
  ): Promise<StoreSnapshot> {
    return (await this.call("store.snapshot", {
      channel,
      community,
    })) as StoreSnapshot;
  }

  async storeThread(
    channel: string,
    root: string,
    community?: string
  ): Promise<ThreadSnapshot> {
    return (await this.call("store.thread", {
      channel,
      root,
      community,
    })) as ThreadSnapshot;
  }

  async channelSearch(
    query: string,
    community?: string
  ): Promise<{ results: SearchResult[] }> {
    return (await this.call("channel.search", {
      query,
      community,
    })) as { results: SearchResult[] };
  }

  async canvasSet(
    channel: string,
    content: string,
    baseRevision?: string,
    community?: string
  ): Promise<{ saved: boolean }> {
    return (await this.call("canvas.set", {
      channel,
      content,
      base_revision: baseRevision,
      community,
    })) as { saved: boolean };
  }

  async messageDelete(
    channel: string,
    target: string,
    community?: string
  ): Promise<{ deleted: boolean }> {
    return (await this.call("message.delete", {
      channel,
      target,
      community,
    })) as { deleted: boolean };
  }

  async storeMembers(
    channel: string,
    community?: string
  ): Promise<{ channel: string; members: Member[] }> {
    return (await this.call("store.members", {
      channel,
      community,
    })) as { channel: string; members: Member[] };
  }

  shutdown(): void {
    if (this.proc) {
      this.proc.kill();
      this.proc = null;
    }
    for (const [, { reject }] of this.pending) {
      reject(new Error("daemon shutdown"));
    }
    this.pending.clear();
  }
}
