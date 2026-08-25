import React from "react";
import { Box, Text } from "ink";
import { useTheme } from "../theme.ts";

interface Props {
  value: string;
  channelName: string;
}

export function Composer({ value, channelName }: Props) {
  const t = useTheme();
  return (
    <Box borderStyle="single" borderColor={t.composerBorder} paddingX={1}>
      <Text color={t.composerPrompt}>{`#${channelName} ❯ `}</Text>
      <Text>{value || " "}</Text>
      <Text color={t.composerCursor}>▏</Text>
    </Box>
  );
}
