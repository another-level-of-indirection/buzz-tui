import React from "react";
import { Box, Text } from "ink";
import { useTheme } from "../theme.ts";
import type { Member } from "../../../protocol/index.ts";

interface Props {
  members: Member[];
  channelName: string;
  onClose: () => void;
}

export function MemberPane({ members, channelName, onClose }: Props) {
  const t = useTheme();
  return (
    <Box flexDirection="column" width={28} borderStyle="single" borderColor={t.memberBorder} overflow="hidden">
      <Box paddingX={1} justifyContent="space-between">
        <Text bold color={t.memberHeader}>Members ({members.length})</Text>
        <Text color={t.helpText} dimColor>esc ✕</Text>
      </Box>
      <Box flexDirection="column" paddingX={1}>
        {members.length === 0 ? (
          <Text color={t.emptyText} italic>Loading…</Text>
        ) : (
          members.map((m) => (
            <Box key={m.pubkey} gap={1}>
              <Text color={m.is_me ? t.memberSelf : t.memberOther}>{m.name}</Text>
              {m.is_me && <Text color={t.helpText} dimColor>(you)</Text>}
            </Box>
          ))
        )}
      </Box>
    </Box>
  );
}
