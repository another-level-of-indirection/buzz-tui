import React from "react";
import { Box, Text } from "ink";
import { useTheme } from "../theme.ts";
import type { Message } from "../../../protocol/index.ts";

interface Props {
  messages: Message[];
  rootId: string;
  selectedIndex?: number;
  onClose: () => void;
}

function formatTime(ts: number): string {
  const d = new Date(ts * 1000);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function ThreadPane({ messages, rootId, selectedIndex = 0, onClose }: Props) {
  const t = useTheme();
  return (
    <Box flexDirection="column" width={40} borderStyle="single" borderColor={t.threadBorder}>
      <Box paddingX={1} justifyContent="space-between">
        <Text bold color={t.threadHeader}>Thread ({messages.length})</Text>
        <Text color={t.helpText} dimColor>↑↓ select · ^R reply · esc ✕</Text>
      </Box>
      <Box flexDirection="column" flexGrow={1} paddingX={1}>
        {messages.length === 0 ? (
          <Text color={t.emptyText} italic>Loading…</Text>
        ) : (
          messages.map((msg, i) => {
            const isRoot = i === 0;
            const isSelected = i === selectedIndex;
            return (
              <Box key={msg.id} flexDirection="column">
                <Box gap={1}>
                  <Text color={isSelected ? t.threadSelector : t.border} bold={isSelected}>
                    {isSelected ? "▸" : " "}
                  </Text>
                  <Text color={t.timestamp} dimColor>{formatTime(msg.created_at)}</Text>
                  <Text color={isRoot ? t.threadRootAuthor : t.authorName} bold={isSelected}>
                    {msg.author_name}
                  </Text>
                  {msg.edited && <Text color={t.editedMark} dimColor>(edited)</Text>}
                </Box>
                <Box paddingLeft={14}>
                  <Text>{msg.content}</Text>
                </Box>
                {msg.reactions.length > 0 && (
                  <Box gap={1} paddingLeft={14}>
                    {msg.reactions.map((r, ri) => (
                      <Text key={ri} color={r.mine ? t.reactionMine : t.reactionOther}>
                        {r.emoji} {r.count}
                      </Text>
                    ))}
                  </Box>
                )}
              </Box>
            );
          })
        )}
      </Box>
    </Box>
  );
}
