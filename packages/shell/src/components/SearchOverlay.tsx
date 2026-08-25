import React from "react";
import { Box, Text } from "ink";
import { useTheme } from "../theme.ts";
import type { SearchResult } from "../../../protocol/index.ts";

interface Props {
  query: string;
  results: SearchResult[];
  selectedIndex: number;
  searching: boolean;
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

export function SearchOverlay({ query, results, selectedIndex, searching }: Props) {
  const t = useTheme();
  return (
    <Box flexDirection="column" borderStyle="single" borderColor={t.searchBorder} width="100%">
      <Box paddingX={1}>
        <Text bold color={t.searchHeader}>Search: </Text>
        <Text>{query}</Text>
        <Text color={t.composerCursor}>▏</Text>
      </Box>
      {searching && (
        <Box paddingX={1}>
          <Text color={t.emptyText} italic>Searching…</Text>
        </Box>
      )}
      {results.length > 0 && (
        <Box flexDirection="column" paddingX={1}>
          {results.slice(0, 10).map((r, i) => (
            <Box key={r.id} gap={1}>
              <Text
                color={i === selectedIndex ? t.searchSelected : t.timestamp}
                bold={i === selectedIndex}
                inverse={i === selectedIndex}
              >
                {formatTime(r.created_at)}
              </Text>
              <Text color={i === selectedIndex ? t.authorName : t.channelDefault} bold={i === selectedIndex}>
                {r.author_name}
              </Text>
              <Text color={i === selectedIndex ? t.messageText : t.channelDefault}>
                {r.content.length > 60 ? r.content.slice(0, 60) + "…" : r.content}
              </Text>
            </Box>
          ))}
          {results.length > 10 && (
            <Text color={t.helpText}>…and {results.length - 10} more</Text>
          )}
        </Box>
      )}
      {!searching && results.length === 0 && query.length > 0 && (
        <Box paddingX={1}>
          <Text color={t.emptyText} italic>No results</Text>
        </Box>
      )}
    </Box>
  );
}
