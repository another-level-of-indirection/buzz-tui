/**
 * Keymap configuration — maps key combos to actions.
 *
 * Default bindings match Ian's TUI. Users can override by placing a
 * keymap.json in ~/.config/buzz-tui/.
 */

import { readFileSync } from "fs";
import { join } from "path";
import { homedir } from "os";

export type Action =
  | "quit"
  | "channel_next"
  | "channel_prev"
  | "send"
  | "search"
  | "canvas"
  | "thread_newest"
  | "thread_reply"
  | "close"
  | "scroll_up"
  | "scroll_down";

interface KeyCombo {
  key?: string;
  ctrl?: boolean;
  shift?: boolean;
  meta?: boolean;
  tab?: boolean;
  escape?: boolean;
  return?: boolean;
  upArrow?: boolean;
  downArrow?: boolean;
  pageUp?: boolean;
  pageDown?: boolean;
}

interface Binding {
  combo: KeyCombo;
  action: Action;
}

const DEFAULT_BINDINGS: Binding[] = [
  { combo: { key: "c", ctrl: true }, action: "quit" },
  { combo: { tab: true }, action: "channel_next" },
  { combo: { tab: true, shift: true }, action: "channel_prev" },
  { combo: { return: true }, action: "send" },
  { combo: { key: "f", ctrl: true }, action: "search" },
  { combo: { key: "g", ctrl: true }, action: "canvas" },
  { combo: { key: "t", ctrl: true }, action: "thread_newest" },
  { combo: { key: "r", ctrl: true }, action: "thread_reply" },
  { combo: { escape: true }, action: "close" },
  { combo: { pageUp: true }, action: "scroll_up" },
  { combo: { pageDown: true }, action: "scroll_down" },
];

export function loadKeymap(): Binding[] {
  const configPath = join(
    homedir(),
    ".config",
    "buzz-tui",
    "keymap.json"
  );
  try {
    const raw = readFileSync(configPath, "utf-8");
    const overrides = JSON.parse(raw) as Binding[];
    return [...overrides, ...DEFAULT_BINDINGS];
  } catch {
    return DEFAULT_BINDINGS;
  }
}

export function matchAction(
  bindings: Binding[],
  ch: string,
  key: {
    ctrl: boolean;
    shift: boolean;
    meta: boolean;
    tab: boolean;
    escape: boolean;
    return: boolean;
    upArrow: boolean;
    downArrow: boolean;
    pageUp: boolean;
    pageDown: boolean;
    backspace: boolean;
    delete: boolean;
  }
): Action | null {
  for (const binding of bindings) {
    const c = binding.combo;
    if (c.key && c.key !== ch) continue;
    if (c.ctrl && !key.ctrl) continue;
    if (c.shift && !key.shift) continue;
    if (c.meta && !key.meta) continue;
    if (c.tab && !key.tab) continue;
    if (c.escape && !key.escape) continue;
    if (c.return && !key.return) continue;
    if (c.upArrow && !key.upArrow) continue;
    if (c.downArrow && !key.downArrow) continue;
    if (c.pageUp && !key.pageUp) continue;
    if (c.pageDown && !key.pageDown) continue;
    return binding.action;
  }
  return null;
}
