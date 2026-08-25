/**
 * git-status plugin — shows `git status --short` in a side pane.
 *
 * Refreshes every 5 seconds. Demonstrates the plugin SDK lifecycle,
 * pane registration, and slash command integration.
 */

import React, { useState, useEffect } from "react";
import { Box, Text } from "ink";
import type { PluginFactory, BuzzPluginAPI } from "../../packages/plugin-sdk/index.ts";

interface GitLine {
  status: string;
  file: string;
}

function GitPane() {
  const [lines, setLines] = useState<GitLine[]>([]);
  const [branch, setBranch] = useState("...");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function refresh() {
      try {
        const branchProc = Bun.spawn(["git", "branch", "--show-current"], {
          stdout: "pipe",
          stderr: "pipe",
        });
        const branchOut = await new Response(branchProc.stdout).text();
        if (!cancelled) setBranch(branchOut.trim() || "detached");

        const statusProc = Bun.spawn(["git", "status", "--short"], {
          stdout: "pipe",
          stderr: "pipe",
        });
        const statusOut = await new Response(statusProc.stdout).text();
        if (!cancelled) {
          const parsed = statusOut
            .trim()
            .split("\n")
            .filter(Boolean)
            .map((line) => ({
              status: line.slice(0, 2).trim(),
              file: line.slice(3),
            }));
          setLines(parsed);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(String(e));
      }
    }

    refresh();
    const interval = setInterval(refresh, 5000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, []);

  return (
    <Box flexDirection="column" borderStyle="single" borderColor="blue" width={36}>
      <Box paddingX={1}>
        <Text bold color="blue">
          Git — {branch}
        </Text>
      </Box>
      <Box flexDirection="column" paddingX={1}>
        {error ? (
          <Text color="red">{error}</Text>
        ) : lines.length === 0 ? (
          <Text color="green" italic>
            Clean working tree
          </Text>
        ) : (
          lines.slice(0, 20).map((l, i) => (
            <Box key={i} gap={1}>
              <Text color={l.status.includes("M") ? "yellow" : l.status.includes("?") ? "red" : "green"}>
                {l.status.padEnd(2)}
              </Text>
              <Text>{l.file}</Text>
            </Box>
          ))
        )}
      </Box>
    </Box>
  );
}

const factory: PluginFactory = () => ({
  activate(api: BuzzPluginAPI) {
    let visible = false;

    function toggle() {
      visible = !visible;
      api.ui.setPane("git-status", visible ? <GitPane /> : null);
    }

    api.ui.registerCommand("git", () => toggle());
    api.ui.showNotice("Git status: /git or Ctrl-Shift-G to toggle");

    return () => {
      api.ui.setPane("git-status", null);
    };
  },
});

export default factory;
