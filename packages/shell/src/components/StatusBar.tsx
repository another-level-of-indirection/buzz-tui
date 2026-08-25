import React from "react";
import { Box, Text } from "ink";
import { useTheme } from "../theme.ts";

interface Props {
  pubkey: string;
  community: string;
  connected: boolean;
}

export function StatusBar({ pubkey, community, connected }: Props) {
  const t = useTheme();
  return (
    <Box paddingX={1} justifyContent="space-between">
      <Text color={t.statusBar}>{community}</Text>
      <Text color={connected ? t.statusConnected : t.statusDisconnected}>
        {connected ? "●" : "○"} {pubkey.slice(0, 8)}
      </Text>
    </Box>
  );
}
