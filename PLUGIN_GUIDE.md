# Writing a buzz-tui Plugin

Plugins extend the TUI with custom panes, slash commands, and keybindings — without touching Rust. A plugin is a directory with a `manifest.json` and a TypeScript entry module.

## Quick start

```
plugins/my-plugin/
  manifest.json
  index.tsx
  tsconfig.json
  package.json
```

### manifest.json

```json
{
  "name": "my-plugin",
  "version": "0.1.0",
  "description": "What it does",
  "entry": "index.tsx",
  "panes": [
    { "id": "my-pane", "title": "My Pane", "position": "right" }
  ],
  "commands": [
    { "name": "myplugin", "description": "Toggle my pane" }
  ]
}
```

### index.tsx

```tsx
import React, { useState, useEffect } from "react";
import { Box, Text } from "ink";
import type { PluginFactory, BuzzPluginAPI } from "../../packages/plugin-sdk/index.ts";

function MyPane() {
  return (
    <Box flexDirection="column" borderStyle="single" borderColor="blue" width={30}>
      <Box paddingX={1}>
        <Text bold color="blue">My Plugin</Text>
      </Box>
      <Box paddingX={1}>
        <Text>Hello from a plugin!</Text>
      </Box>
    </Box>
  );
}

const factory: PluginFactory = () => ({
  activate(api: BuzzPluginAPI) {
    let visible = false;

    api.ui.registerCommand("myplugin", () => {
      visible = !visible;
      api.ui.setPane("my-pane", visible ? <MyPane /> : null);
    });

    api.ui.showNotice("My plugin loaded! Use /myplugin");

    return () => {
      api.ui.setPane("my-pane", null);
    };
  },
});

export default factory;
```

### tsconfig.json

```json
{
  "compilerOptions": {
    "jsx": "react",
    "esModuleInterop": true,
    "module": "ESNext",
    "moduleResolution": "bundler",
    "allowImportingTsExtensions": true,
    "noEmit": true
  }
}
```

### package.json

```json
{
  "name": "@buzz-tui/plugin-my-plugin",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "dependencies": {
    "react": "^19.2.8",
    "ink": "^7.1.1",
    "@types/react": "^19.2.18"
  }
}
```

Then `bun install` from the repo root to link dependencies.

## Plugin API

### `api.pubkey`
Your identity's public key (hex). Never the private key.

### `api.community`
The active community URL.

### `api.focusedChannel`
Currently focused channel ID, or null.

### `api.buzz` (read-only)
- `channelList()` — list of channels
- `storeSnapshot(channel)` — messages and typing indicators
- `storeThread(channel, root)` — thread messages
- `canvasGet(channel)` — channel canvas content
- `channelSearch(query)` — NIP-50 search results

### `api.actions` (write)
- `messageSend(channel, content)` — send a message
- `messageReply(channel, content, replyTo)` — reply in a thread
- `messageReact(channel, target, emoji)` — add a reaction

### `api.ui`
- `showNotice(message)` — show a transient notice
- `setPane(paneId, element)` — render a React element in a pane slot (pass null to hide)
- `registerCommand(name, handler)` — register a `/slash` command

## Guidelines

1. **No secret access.** Plugins never see private keys — only pubkeys and display names.
2. **Use `tsconfig.json` with `"jsx": "react"`** to avoid Bun's automatic JSX transform cache issues.
3. **Declare `react` and `ink` as dependencies** in your `package.json`.
4. **Return a cleanup function** from `activate()` to clean up panes and timers.
5. **Pane width.** Side panes should set a fixed `width` (24-40). The main transcript takes remaining space.

## Examples

See `plugins/git-status/` and `plugins/scratch-notes/` for complete working examples.
