import React from "react";
import { Box, Text } from "ink";
import { useTheme } from "../theme.ts";
import type { Message, TypingUser } from "../../../protocol/index.ts";

interface Props {
  messages: Message[];
  typing: TypingUser[];
  channelName: string;
  selectedIndex?: number;
}

function formatTime(ts: number): string {
  const d = new Date(ts * 1000);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function Transcript({ messages, typing, channelName, selectedIndex }: Props) {
  const t = useTheme();
  return (
    <Box flexDirection="column" flexGrow={1} borderStyle="single" borderColor={t.border}>
      <Box paddingX={1}>
        <Text bold color={t.transcriptHeader}>#{channelName}</Text>
      </Box>
      <Box flexDirection="column" flexGrow={1} paddingX={1}>
        {messages.length === 0 ? (
          <Text color={t.emptyText} italic>No messages yet</Text>
        ) : (
          messages.slice(-30).map((msg, idx) => {
            const visibleIdx = messages.length <= 30 ? idx : idx + (messages.length - 30);
            const isSelected = selectedIndex !== undefined && visibleIdx === selectedIndex;
            return (
              <Box key={msg.id} flexDirection="column">
                <Box gap={1}>
                  {selectedIndex !== undefined && (
                    <Text color={isSelected ? t.channelSelected : t.border} bold={isSelected}>
                      {isSelected ? "▸" : " "}
                    </Text>
                  )}
                  <Text color={t.timestamp} dimColor>{formatTime(msg.created_at)}</Text>
                  <Text color={t.authorName} bold>{msg.author_name}</Text>
                  {msg.edited && <Text color={t.editedMark} dimColor>(edited)</Text>}
                  {msg.reply_count > 0 && (
                    <Text color={t.replyCount}>
                      [{msg.reply_count} {msg.reply_count === 1 ? "reply" : "replies"}]
                    </Text>
                  )}
                </Box>
                <Box paddingLeft={selectedIndex !== undefined ? 14 : 12}>
                  <Text color={t.messageText}>{msg.content}</Text>
                </Box>
                {msg.reactions.length > 0 && (
                  <Box gap={1} paddingLeft={selectedIndex !== undefined ? 14 : 12}>
                    {msg.reactions.map((r, i) => (
                      <Text key={i} color={r.mine ? t.reactionMine : t.reactionOther}>
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
      {typing.length > 0 && (
        <Box paddingX={1}>
          <Text color={t.typingIndicator} italic>
            {typing.map((t) => t.name).join(", ")}{" "}
            {typing.length === 1 ? "is" : "are"} typing…
          </Text>
        </Box>
      )}
    </Box>
  );
}
