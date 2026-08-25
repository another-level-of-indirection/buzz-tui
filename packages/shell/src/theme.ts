/**
 * Theme system — token-based colors loadable from ~/.config/buzz-tui/theme.json.
 *
 * Every component reads from the current theme context instead of hardcoding
 * Ink color strings. Users override individual tokens; unset tokens fall back
 * to the built-in default.
 */

import React, { createContext, useContext } from "react";
import { readFileSync } from "fs";
import { join } from "path";
import { homedir } from "os";

export interface ThemeTokens {
  // Chrome
  border: string;
  borderFocus: string;
  statusBar: string;
  statusConnected: string;
  statusDisconnected: string;
  helpText: string;
  notice: string;

  // Channel list
  channelHeader: string;
  channelSelected: string;
  channelUnread: string;
  channelDefault: string;
  channelMention: string;

  // Transcript
  transcriptHeader: string;
  timestamp: string;
  authorName: string;
  authorSelf: string;
  messageText: string;
  editedMark: string;
  replyCount: string;
  reactionMine: string;
  reactionOther: string;
  typingIndicator: string;
  emptyText: string;

  // Thread
  threadBorder: string;
  threadHeader: string;
  threadRootAuthor: string;
  threadSelector: string;

  // Canvas
  canvasBorder: string;
  canvasHeader: string;
  canvasRevision: string;

  // Search
  searchBorder: string;
  searchHeader: string;
  searchSelected: string;

  // Members
  memberBorder: string;
  memberHeader: string;
  memberSelf: string;
  memberOther: string;

  // Composer
  composerBorder: string;
  composerPrompt: string;
  composerCursor: string;
}

export const DEFAULT_THEME: ThemeTokens = {
  border: "gray",
  borderFocus: "cyan",
  statusBar: "gray",
  statusConnected: "green",
  statusDisconnected: "red",
  helpText: "gray",
  notice: "yellow",

  channelHeader: "cyan",
  channelSelected: "cyan",
  channelUnread: "white",
  channelDefault: "gray",
  channelMention: "yellow",

  transcriptHeader: "cyan",
  timestamp: "gray",
  authorName: "cyan",
  authorSelf: "yellow",
  messageText: "white",
  editedMark: "gray",
  replyCount: "gray",
  reactionMine: "cyan",
  reactionOther: "gray",
  typingIndicator: "gray",
  emptyText: "gray",

  threadBorder: "yellow",
  threadHeader: "yellow",
  threadRootAuthor: "yellow",
  threadSelector: "white",

  canvasBorder: "magenta",
  canvasHeader: "magenta",
  canvasRevision: "gray",

  searchBorder: "green",
  searchHeader: "green",
  searchSelected: "green",

  memberBorder: "cyan",
  memberHeader: "cyan",
  memberSelf: "yellow",
  memberOther: "cyan",

  composerBorder: "gray",
  composerPrompt: "gray",
  composerCursor: "gray",
};

export function loadTheme(): ThemeTokens {
  const configPath = join(homedir(), ".config", "buzz-tui", "theme.json");
  try {
    const raw = readFileSync(configPath, "utf-8");
    const overrides = JSON.parse(raw) as Partial<ThemeTokens>;
    return { ...DEFAULT_THEME, ...overrides };
  } catch {
    return DEFAULT_THEME;
  }
}

export const ThemeContext = createContext<ThemeTokens>(DEFAULT_THEME);

export function useTheme(): ThemeTokens {
  return useContext(ThemeContext);
}
