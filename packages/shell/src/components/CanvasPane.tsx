import React from "react";
import { Box, Text } from "ink";
import { useTheme } from "../theme.ts";
import type { Canvas } from "../../../protocol/index.ts";

interface Props {
  canvas: Canvas | null;
  channelName: string;
  onClose: () => void;
}

function formatTime(ts: number): string {
  const d = new Date(ts * 1000);
  return d.toLocaleString([], {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function CanvasPane({ canvas, channelName, onClose }: Props) {
  const t = useTheme();
  return (
    <Box flexDirection="column" flexGrow={1} borderStyle="single" borderColor={t.canvasBorder}>
      <Box paddingX={1} justifyContent="space-between">
        <Text bold color={t.canvasHeader}>Canvas — #{channelName}</Text>
        <Text color={t.helpText} dimColor>^E edit · esc ✕</Text>
      </Box>
      <Box flexDirection="column" flexGrow={1} paddingX={1}>
        {canvas ? (
          <>
            <Text color={t.canvasRevision} dimColor>
              Last updated {formatTime(canvas.updated_at)} · revision {canvas.id.slice(0, 8)}
            </Text>
            <Text>{canvas.content || "(empty)"}</Text>
          </>
        ) : (
          <Text color={t.emptyText} italic>No canvas for this channel</Text>
        )}
      </Box>
    </Box>
  );
}
