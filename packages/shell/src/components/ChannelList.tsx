import React from "react";
import { Box, Text } from "ink";
import { useTheme } from "../theme.ts";
import type { Channel } from "../../../protocol/index.ts";

interface Props {
  channels: Channel[];
  selected: number;
  onSelect: (index: number) => void;
}

export function ChannelList({ channels, selected }: Props) {
  const t = useTheme();
  return (
    <Box flexDirection="column" width={24} borderStyle="single" borderColor={t.border}>
      <Box paddingX={1}>
        <Text bold color={t.channelHeader}>Channels</Text>
      </Box>
      {channels.map((ch, i) => {
        const isSelected = i === selected;
        const badge = ch.mentions ? " @" : ch.unread > 0 ? ` ${ch.unread}` : "";
        return (
          <Box key={ch.id} paddingX={1}>
            <Text
              color={
                isSelected
                  ? t.channelSelected
                  : ch.mentions
                    ? t.channelMention
                    : ch.unread > 0
                      ? t.channelUnread
                      : t.channelDefault
              }
              bold={isSelected}
              inverse={isSelected}
            >
              {`#${ch.name}${badge}`}
            </Text>
          </Box>
        );
      })}
    </Box>
  );
}
