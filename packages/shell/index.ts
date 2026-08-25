#!/usr/bin/env bun
/**
 * buzz-tui shell — TypeScript Ink frontend for the hybrid Buzz TUI.
 *
 * Spawns buzz-sessiond and renders the terminal UI.
 */

import React from "react";
import { render } from "ink";
import { SessionDaemon } from "./src/daemon.ts";
import { App } from "./src/app.tsx";
import { loadTheme, ThemeContext } from "./src/theme.ts";

const daemon = new SessionDaemon();
const theme = loadTheme();

const daemonPath = process.env.BUZZ_SESSIOND_PATH ?? "buzz-sessiond";

process.on("SIGINT", () => {
  daemon.shutdown();
  process.exit(0);
});

process.on("SIGTERM", () => {
  daemon.shutdown();
  process.exit(0);
});

await daemon.start(daemonPath);

render(
  React.createElement(
    ThemeContext.Provider,
    { value: theme },
    React.createElement(App, { daemon })
  )
);
