import React, { useState, useEffect, useCallback, useRef, useMemo } from "react";
import { Box, Text, useInput, useApp } from "ink";
import { join } from "path";
import { SessionDaemon } from "./daemon.ts";
import { ChannelList } from "./components/ChannelList.tsx";
import { Transcript } from "./components/Transcript.tsx";
import { Composer } from "./components/Composer.tsx";
import { StatusBar } from "./components/StatusBar.tsx";
import { ThreadPane } from "./components/ThreadPane.tsx";
import { CanvasPane } from "./components/CanvasPane.tsx";
import { SearchOverlay } from "./components/SearchOverlay.tsx";
import { MemberPane } from "./components/MemberPane.tsx";
import { loadKeymap, matchAction } from "./keymap.ts";
import { parseSlashCommand, findCommand, type SlashContext } from "./slash.ts";
import { PluginHost } from "./plugin-host.ts";
import type {
  Channel,
  Message,
  Member,
  TypingUser,
  StoreEvent,
  Canvas,
  SearchResult,
} from "../../protocol/index.ts";

type Mode = "normal" | "search" | "thread-reply" | "canvas-edit" | "message-action";

interface AppProps {
  daemon: SessionDaemon;
}

export function App({ daemon }: AppProps) {
  const { exit } = useApp();
  const [pubkey, setPubkey] = useState("");
  const [community, setCommunity] = useState("");
  const [connected, setConnected] = useState(false);
  const [channels, setChannels] = useState<Channel[]>([]);
  const [selectedChannel, setSelectedChannel] = useState(0);
  const [messages, setMessages] = useState<Message[]>([]);
  const [typing, setTyping] = useState<TypingUser[]>([]);
  const [input, setInput] = useState("");
  const [focusedChannel, setFocusedChannel] = useState<string | null>(null);

  // Thread state
  const [threadRoot, setThreadRoot] = useState<string | null>(null);
  const [threadMessages, setThreadMessages] = useState<Message[]>([]);
  const [threadReplyInput, setThreadReplyInput] = useState("");
  const [threadSelected, setThreadSelected] = useState(0);

  // Canvas state
  const [canvasOpen, setCanvasOpen] = useState(false);
  const [canvas, setCanvas] = useState<Canvas | null>(null);
  const [canvasEditContent, setCanvasEditContent] = useState("");

  // Members state
  const [membersOpen, setMembersOpen] = useState(false);
  const [members, setMembers] = useState<Member[]>([]);

  // Search state
  const [mode, setMode] = useState<Mode>("normal");
  const [searchQuery, setSearchQuery] = useState("");
  const [searchResults, setSearchResults] = useState<SearchResult[]>([]);
  const [searchIndex, setSearchIndex] = useState(0);
  const [searching, setSearching] = useState(false);

  // Message selection state
  const [msgSelected, setMsgSelected] = useState(-1);

  // Notice bar
  const [notice, setNotice] = useState<string | null>(null);

  // Plugin state
  const [pluginPanes, setPluginPanes] = useState<React.ReactElement[]>([]);

  const keymap = useMemo(() => loadKeymap(), []);
  const lastTypingSent = useRef(0);
  const pluginHostRef = useRef<PluginHost | null>(null);

  // Initialize plugin host once we have identity
  useEffect(() => {
    if (!pubkey || !community) return;
    const host = new PluginHost(daemon, pubkey, community, setNotice);
    host.onUpdate(() => {
      setPluginPanes(host.panes.map((p) => p.element));
    });
    pluginHostRef.current = host;

    const pluginsDir = join(
      import.meta.dir,
      "..",
      "..",
      "..",
      "plugins"
    );
    host.loadAll(pluginsDir).catch(() => {});

    return () => {
      host.shutdown();
      pluginHostRef.current = null;
    };
  }, [pubkey, community, daemon]);

  // Keep plugin host in sync with focused channel
  useEffect(() => {
    pluginHostRef.current?.setFocusedChannel(focusedChannel);
  }, [focusedChannel]);

  useEffect(() => {
    const onReady = (data: {
      pubkey: string;
      communities: { url: string; name: string }[];
    }) => {
      setPubkey(data.pubkey);
      if (data.communities.length > 0) {
        setCommunity(data.communities[0].name);
      }
    };

    const onConnected = () => setConnected(true);
    const onDisconnected = () => setConnected(false);

    const onChannelsLoaded = async () => {
      try {
        const chs = await daemon.channelList();
        setChannels(chs);
      } catch {
        /* ignore */
      }
    };

    const onEvent = (evt: StoreEvent) => {
      if (focusedChannel) refreshSnapshot();
      if (threadRoot && focusedChannel) refreshThread();

      // Desktop notification for mentions when terminal is backgrounded
      if (
        (evt.kind === 9 || evt.kind === 11) &&
        evt.content.includes(`@`) &&
        evt.pubkey !== pubkey
      ) {
        try {
          const { execSync } = require("child_process");
          if (process.platform === "darwin") {
            const msg = `${evt.author_name}: ${evt.content.slice(0, 80)}`;
            execSync(
              `osascript -e 'display notification "${msg.replace(/"/g, '\\"')}" with title "Buzz"'`,
              { stdio: "ignore", timeout: 2000 }
            );
          }
        } catch {
          /* best-effort */
        }
      }
    };

    daemon.on("ready", onReady);
    daemon.on("connected", onConnected);
    daemon.on("disconnected", onDisconnected);
    daemon.on("channels_loaded", onChannelsLoaded);
    daemon.on("event", onEvent);

    return () => {
      daemon.off("ready", onReady);
      daemon.off("connected", onConnected);
      daemon.off("disconnected", onDisconnected);
      daemon.off("channels_loaded", onChannelsLoaded);
      daemon.off("event", onEvent);
    };
  }, [daemon, focusedChannel, threadRoot]);

  const refreshSnapshot = useCallback(async () => {
    if (!focusedChannel) return;
    try {
      const snap = await daemon.storeSnapshot(focusedChannel);
      setMessages(snap.messages);
      setTyping(snap.typing);
    } catch {
      /* ignore */
    }
  }, [daemon, focusedChannel]);

  const refreshThread = useCallback(async () => {
    if (!focusedChannel || !threadRoot) return;
    try {
      const snap = await daemon.storeThread(focusedChannel, threadRoot);
      setThreadMessages(snap.messages);
    } catch {
      /* ignore */
    }
  }, [daemon, focusedChannel, threadRoot]);

  const refreshMembers = useCallback(async () => {
    if (!focusedChannel) return;
    try {
      const res = await daemon.storeMembers(focusedChannel);
      setMembers(res.members);
    } catch {
      /* ignore */
    }
  }, [daemon, focusedChannel]);

  // Focus channel when selection changes
  useEffect(() => {
    const ch = channels[selectedChannel];
    if (!ch) return;
    if (ch.id === focusedChannel) return;

    setFocusedChannel(ch.id);
    setMessages([]);
    setTyping([]);
    setThreadRoot(null);
    setThreadMessages([]);
    setCanvasOpen(false);
    setCanvas(null);
    setMembersOpen(false);
    setMembers([]);

    (async () => {
      try {
        await daemon.channelFocus(ch.id);
        const snap = await daemon.storeSnapshot(ch.id);
        setMessages(snap.messages);
        setTyping(snap.typing);
      } catch {
        /* ignore */
      }
    })();
  }, [selectedChannel, channels, daemon]);

  // Refresh thread when root changes
  useEffect(() => {
    if (threadRoot && focusedChannel) {
      refreshThread();
    }
  }, [threadRoot, focusedChannel]);

  // Periodic refresh
  useEffect(() => {
    const interval = setInterval(() => {
      refreshSnapshot();
      if (threadRoot) refreshThread();
    }, 2000);
    return () => clearInterval(interval);
  }, [refreshSnapshot, refreshThread, threadRoot]);

  const openThread = useCallback((rootId: string) => {
    setThreadRoot(rootId);
    setThreadMessages([]);
    setThreadReplyInput("");
    setThreadSelected(0);
  }, []);

  const closeThread = useCallback(() => {
    setThreadRoot(null);
    setThreadMessages([]);
    setThreadReplyInput("");
    setThreadSelected(0);
    if (mode === "thread-reply") setMode("normal");
  }, [mode]);

  const toggleCanvas = useCallback(async () => {
    if (canvasOpen) {
      setCanvasOpen(false);
      if (mode === "canvas-edit") setMode("normal");
      return;
    }
    if (!focusedChannel) return;
    setCanvasOpen(true);
    try {
      const c = await daemon.canvasGet(focusedChannel);
      setCanvas(c);
    } catch {
      /* ignore */
    }
  }, [canvasOpen, focusedChannel, daemon, mode]);

  const toggleMembers = useCallback(async () => {
    if (membersOpen) {
      setMembersOpen(false);
      return;
    }
    setMembersOpen(true);
    await refreshMembers();
  }, [membersOpen, refreshMembers]);

  const runSearch = useCallback(
    async (query: string) => {
      if (!query.trim()) return;
      setSearching(true);
      try {
        const res = await daemon.channelSearch(query.trim());
        setSearchResults(res.results);
        setSearchIndex(0);
      } catch {
        /* ignore */
      }
      setSearching(false);
    },
    [daemon]
  );

  const sendTypingThrottled = useCallback(() => {
    const now = Date.now();
    if (now - lastTypingSent.current < 3000) return;
    lastTypingSent.current = now;
    if (focusedChannel) {
      daemon.typingSet(focusedChannel).catch(() => {});
    }
  }, [focusedChannel, daemon]);

  useInput((ch, key) => {
    if (key.ctrl && ch === "c") {
      pluginHostRef.current?.shutdown();
      daemon.shutdown();
      exit();
      return;
    }

    // ── Search mode ───────────────────────────────────────────────────────
    if (mode === "search") {
      if (key.escape) {
        setMode("normal");
        setSearchQuery("");
        setSearchResults([]);
        return;
      }
      if (key.return) {
        runSearch(searchQuery);
        return;
      }
      if (key.downArrow) {
        setSearchIndex((i) => Math.min(i + 1, searchResults.length - 1));
        return;
      }
      if (key.upArrow) {
        setSearchIndex((i) => Math.max(i - 1, 0));
        return;
      }
      if (key.backspace || key.delete) {
        setSearchQuery((q) => q.slice(0, -1));
        return;
      }
      if (ch && !key.ctrl && !key.meta) {
        setSearchQuery((q) => q + ch);
        return;
      }
      return;
    }

    // ── Canvas edit mode ──────────────────────────────────────────────────
    if (mode === "canvas-edit") {
      if (key.escape) {
        setMode("normal");
        return;
      }
      // Ctrl-S saves
      if (key.ctrl && ch === "s") {
        if (focusedChannel && canvasEditContent.trim()) {
          daemon
            .canvasSet(
              focusedChannel,
              canvasEditContent,
              canvas?.id
            )
            .then(() => {
              setNotice("Canvas saved");
              setMode("normal");
              daemon.canvasGet(focusedChannel!).then(setCanvas).catch(() => {});
            })
            .catch((e) => {
              setNotice(`Canvas save failed: ${e.message}`);
            });
        }
        return;
      }
      if (key.return) {
        setCanvasEditContent((v) => v + "\n");
        return;
      }
      if (key.backspace || key.delete) {
        setCanvasEditContent((v) => v.slice(0, -1));
        return;
      }
      if (ch && !key.ctrl && !key.meta) {
        setCanvasEditContent((v) => v + ch);
        return;
      }
      return;
    }

    // ── Thread reply mode ─────────────────────────────────────────────────
    if (mode === "thread-reply") {
      if (key.escape) {
        setMode("normal");
        return;
      }
      if (key.return) {
        if (threadReplyInput.trim() && focusedChannel && threadRoot) {
          // Reply to selected message in thread, or root if first selected
          const replyTarget =
            threadMessages[threadSelected]?.id ?? threadRoot;
          daemon
            .messageReply(
              focusedChannel,
              threadReplyInput.trim(),
              replyTarget
            )
            .catch(() => {});
          setThreadReplyInput("");
          setMode("normal");
        }
        return;
      }
      if (key.backspace || key.delete) {
        setThreadReplyInput((v) => v.slice(0, -1));
        return;
      }
      if (ch && !key.ctrl && !key.meta) {
        setThreadReplyInput((v) => v + ch);
        return;
      }
      return;
    }

    // ── Message action mode ─────────────────────────────────────────────
    if (mode === "message-action") {
      if (key.escape) {
        setMode("normal");
        setMsgSelected(-1);
        return;
      }
      if (key.upArrow) {
        setMsgSelected((i) => Math.max(i - 1, 0));
        return;
      }
      if (key.downArrow) {
        setMsgSelected((i) => Math.min(i + 1, messages.length - 1));
        return;
      }
      const sel = messages[msgSelected];
      if (!sel) return;
      // 't' - open thread on selected message
      if (ch === "t" && sel.reply_count > 0) {
        openThread(sel.id);
        setMode("normal");
        return;
      }
      // 'r' - reply to selected message
      if (ch === "r" && focusedChannel) {
        openThread(sel.id);
        setMode("thread-reply");
        return;
      }
      // 'e' - react with thumbs up
      if (ch === "e" && focusedChannel) {
        daemon.messageReact(focusedChannel, sel.id, "+1").catch(() => {});
        setMode("normal");
        setMsgSelected(-1);
        return;
      }
      // 'd' - delete own message
      if (ch === "d" && focusedChannel && sel.author === pubkey) {
        daemon.messageDelete(focusedChannel, sel.id).catch(() => {});
        setMode("normal");
        setMsgSelected(-1);
        return;
      }
      return;
    }

    // ── Normal mode ───────────────────────────────────────────────────────
    if (key.escape) {
      if (msgSelected >= 0) {
        setMsgSelected(-1);
        return;
      }
      if (canvasOpen) {
        setCanvasOpen(false);
        return;
      }
      if (membersOpen) {
        setMembersOpen(false);
        return;
      }
      if (threadRoot) {
        closeThread();
        return;
      }
      return;
    }

    if (key.ctrl && ch === "f") {
      setMode("search");
      setSearchQuery("");
      setSearchResults([]);
      return;
    }

    if (key.ctrl && ch === "g") {
      toggleCanvas();
      return;
    }

    // Ctrl-E: edit canvas
    if (key.ctrl && ch === "e") {
      if (canvasOpen) {
        setCanvasEditContent(canvas?.content ?? "");
        setMode("canvas-edit");
      }
      return;
    }

    // Ctrl-M: toggle members
    if (key.ctrl && ch === "m") {
      toggleMembers();
      return;
    }

    // Ctrl-J: enter message selection/action mode
    if (key.ctrl && ch === "j") {
      if (messages.length > 0) {
        setMsgSelected(messages.length - 1);
        setMode("message-action");
      }
      return;
    }

    // Page Up: load older history
    if (key.pageUp && focusedChannel) {
      const oldest = messages[0];
      if (oldest) {
        daemon
          .channelHistory(focusedChannel, { before: oldest.created_at })
          .then(() => refreshSnapshot())
          .catch(() => {});
      }
      return;
    }

    // Ctrl-T: open thread — find newest message with replies
    if (key.ctrl && ch === "t") {
      const lastWithReplies = [...messages]
        .reverse()
        .find((m) => m.reply_count > 0);
      if (lastWithReplies && focusedChannel) {
        openThread(lastWithReplies.id);
      }
      return;
    }

    // Ctrl-R: reply in thread
    if (key.ctrl && ch === "r") {
      if (threadRoot) {
        setMode("thread-reply");
      }
      return;
    }

    // Arrow keys in thread pane to select message
    if (threadRoot && key.upArrow) {
      setThreadSelected((i) => Math.max(i - 1, 0));
      return;
    }
    if (threadRoot && key.downArrow) {
      setThreadSelected((i) =>
        Math.min(i + 1, threadMessages.length - 1)
      );
      return;
    }

    // Tab: cycle channels
    if (key.tab && !key.shift) {
      setSelectedChannel((prev) =>
        prev < channels.length - 1 ? prev + 1 : 0
      );
      return;
    }
    if (key.tab && key.shift) {
      setSelectedChannel((prev) =>
        prev > 0 ? prev - 1 : channels.length - 1
      );
      return;
    }

    // Enter: send or slash command
    if (key.return) {
      if (input.trim() && focusedChannel) {
        const parsed = parseSlashCommand(input.trim());
        if (parsed) {
          // Check plugin commands first
          const pluginCmd = pluginHostRef.current?.commands.get(parsed.name);
          if (pluginCmd) {
            Promise.resolve(pluginCmd.handler(parsed.args)).catch(() => {});
            setInput("");
            return;
          }
          const cmd = findCommand(parsed.name);
          if (cmd) {
            const ctx: SlashContext = {
              channelId: focusedChannel,
              daemon: daemon as SlashContext["daemon"],
              setNotice: (msg: string) => setNotice(msg),
              openSearch: (q: string) => {
                setMode("search");
                setSearchQuery(q);
                runSearch(q);
              },
              toggleCanvas: () => {
                toggleCanvas();
              },
            };
            Promise.resolve(cmd.execute(parsed.args, ctx)).catch(() => {});
          } else {
            setNotice(`Unknown command: /${parsed.name}. Try /help`);
          }
          setInput("");
        } else {
          daemon.messageSend(focusedChannel, input.trim()).catch(() => {});
          setInput("");
        }
      }
      return;
    }

    if (key.backspace || key.delete) {
      setInput((prev) => prev.slice(0, -1));
      return;
    }

    if (ch && !key.ctrl && !key.meta) {
      setInput((prev) => prev + ch);
      if (notice) setNotice(null);
      sendTypingThrottled();
    }
  });

  const currentChannel = channels[selectedChannel];
  const channelName = currentChannel?.name ?? "…";

  if (!pubkey) {
    return (
      <Box flexDirection="column" padding={1}>
        <Text color="yellow">Connecting to buzz-sessiond…</Text>
      </Box>
    );
  }

  return (
    <Box flexDirection="column" height="100%">
      <StatusBar pubkey={pubkey} community={community} connected={connected} />

      {mode === "search" && (
        <SearchOverlay
          query={searchQuery}
          results={searchResults}
          selectedIndex={searchIndex}
          searching={searching}
        />
      )}

      <Box flexGrow={1}>
        <ChannelList
          channels={channels}
          selected={selectedChannel}
          onSelect={setSelectedChannel}
        />
        {canvasOpen ? (
          <CanvasPane
            canvas={canvas}
            channelName={channelName}
            onClose={() => setCanvasOpen(false)}
          />
        ) : (
          <Transcript
            messages={messages}
            typing={typing}
            channelName={channelName}
            selectedIndex={mode === "message-action" ? msgSelected : undefined}
          />
        )}
        {threadRoot && !canvasOpen && (
          <ThreadPane
            messages={threadMessages}
            rootId={threadRoot}
            selectedIndex={threadSelected}
            onClose={closeThread}
          />
        )}
        {membersOpen && !canvasOpen && !threadRoot && (
          <MemberPane
            members={members}
            channelName={channelName}
            onClose={() => setMembersOpen(false)}
          />
        )}
        {pluginPanes.map((pane, i) => (
          <React.Fragment key={i}>{pane}</React.Fragment>
        ))}
      </Box>

      {mode === "canvas-edit" ? (
        <Box borderStyle="single" borderColor="magenta" paddingX={1} flexDirection="column">
          <Box justifyContent="space-between">
            <Text color="magenta" bold>
              Editing canvas
            </Text>
            <Text color="gray" dimColor>
              ^S save · Esc cancel
            </Text>
          </Box>
          <Text>
            {canvasEditContent || " "}
            <Text color="gray">▏</Text>
          </Text>
        </Box>
      ) : mode === "thread-reply" && threadRoot ? (
        <Box borderStyle="single" borderColor="yellow" paddingX={1}>
          <Text color="yellow">
            reply to {threadMessages[threadSelected]?.author_name ?? "…"} ❯{" "}
          </Text>
          <Text>{threadReplyInput || " "}</Text>
          <Text color="gray">▏</Text>
        </Box>
      ) : (
        <Composer value={input} channelName={channelName} />
      )}

      {notice && (
        <Box paddingX={1}>
          <Text color="yellow">{notice}</Text>
        </Box>
      )}

      <Box paddingX={1}>
        <Text color="gray" dimColor>
          {mode === "message-action"
            ? "↑↓ select · t thread · r reply · e react · d delete · Esc cancel"
            : "Tab ch · ^J select · ^T thread · ^G canvas · ^E edit · ^M members · ^F search · PgUp history · /help · ^C quit"}
        </Text>
      </Box>
    </Box>
  );
}
