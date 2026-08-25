---
name: GUI Strategy Plan
overview: Extend buzz-tui into a multi-surface monorepo with a shared React web UI served both standalone in-browser and embedded in a Tauri desktop shell, reusing the existing Rust session library and plugin data API across all surfaces.
todos:
  - id: extract-theme
    content: Extract shared theme system into packages/theme/ with TUI (Ink) and DOM (CSS vars) adapters
    status: pending
  - id: refactor-plugin-sdk
    content: Refactor plugin-sdk to separate data/action API from UI rendering; create plugin-sdk-dom adapter
    status: pending
  - id: scaffold-web
    content: Scaffold apps/web/ (Vite + React 19) and build packages/ui/ DOM component library
    status: pending
  - id: daemon-bridge
    content: Build packages/daemon-bridge/ — WebSocket-to-stdio adapter for local-mode browser connections
    status: pending
  - id: scaffold-tauri
    content: Scaffold apps/desktop/ with Tauri 2, implement command handlers wrapping buzz-session
    status: pending
  - id: session-wasm
    content: Create crates/buzz-session-wasm/ for browser-native mode (Phase 4)
    status: pending
  - id: transport-abstraction
    content: Add SessionTransport interface to packages/protocol/ with stdio, WebSocket, Tauri IPC, and WASM implementations
    status: pending
  - id: plugin-dual-surface
    content: Add domEntry support to plugin system and port example plugins
    status: pending
isProject: false
---

# Multi-Surface GUI Strategy for buzz-tui

## Recommendation: Integrated Monorepo

Keep everything in the current repo. The existing workspace structure (`crates/`, `packages/`, `plugins/`) already separates concerns cleanly. Adding `apps/web/` and `apps/desktop/` follows the same pattern and maximizes code sharing.

**Why not separate repos:**

- The Rust `buzz-session` crate is shared by Tauri (directly) and the daemon (for TUI + local-mode browser)
- The plugin SDK, protocol types, and theme tokens are shared across all surfaces
- Coordinated releases and a single CI pipeline avoid version drift
- Plugins authored once can target multiple surfaces from one source tree

---

## Proposed Monorepo Layout

```
buzz-tui/
  crates/
    buzz-session/          # (existing) core Rust library
    buzz-sessiond/         # (existing) stdio JSON-RPC daemon
    buzz-session-wasm/     # NEW: wasm-pack build of session for browser-native mode
  packages/
    protocol/              # (existing) Zod schemas for RPC types
    shell/                 # (existing) Ink TUI
    plugin-sdk/            # (refactored) shared data API, no Ink dependency
    plugin-sdk-ink/        # NEW: Ink UI adapter for TUI plugins
    plugin-sdk-dom/        # NEW: DOM React UI adapter for web/desktop plugins
    theme/                 # NEW: shared token definitions + surface adapters
    ui/                    # NEW: shared React (DOM) component library
  apps/
    web/                   # NEW: Vite + React SPA (browser GUI)
    desktop/               # NEW: Tauri app wrapping the web UI
  plugins/                 # (existing) — add optional renderDOM exports
```

---

## Architecture Overview

```mermaid
graph TD
  subgraph surfaces [User-Facing Surfaces]
    TUI[TUI - Ink Shell]
    Web[Browser GUI - React DOM]
    Desktop[Desktop GUI - Tauri + React DOM]
  end

  subgraph shared [Shared Packages]
    Protocol[packages/protocol]
    PluginSDK[packages/plugin-sdk]
    Theme[packages/theme]
    UILib[packages/ui]
  end

  subgraph backends [Backend / Session]
    Daemon[buzz-sessiond - stdio]
    WASM[buzz-session-wasm]
    TauriBackend[Tauri Rust backend]
  end

  Relay[Buzz Relay]

  TUI --> Protocol
  TUI --> PluginSDK
  Web --> Protocol
  Web --> PluginSDK
  Web --> UILib
  Desktop --> UILib
  Desktop --> Protocol
  Desktop --> PluginSDK

  TUI -->|"JSON-RPC stdio"| Daemon
  Web -->|"JSON-RPC WebSocket"| Daemon
  Web -->|"direct NIP-01"| WASM
  Desktop -->|"Tauri IPC"| TauriBackend

  Daemon --> Relay
  WASM --> Relay
  TauriBackend --> Relay
end
```

---

## Layer-by-Layer Breakdown

### 1. Shared Theme System (`packages/theme/`)

- Token definitions in a single `tokens.json` (40+ existing tokens)
- **TUI adapter**: maps tokens to Ink color strings (existing logic from `packages/shell/src/theme.ts`)
- **DOM adapter**: generates CSS custom properties from the same tokens
- User's `~/.config/buzz-tui/theme.json` applies to all surfaces identically

### 2. Shared Component Library (`packages/ui/`)

- React DOM components that mirror TUI panes: `ChannelList`, `Transcript`, `Composer`, `ThreadPane`, `CanvasPane`, `MemberPane`, `SearchOverlay`, `StatusBar`
- Styled via CSS custom properties from `packages/theme/`
- Shared by both `apps/web/` and `apps/desktop/` (Tauri renders the same web content)
- No terminal dependencies (Ink stays in `packages/shell/`)

### 3. Plugin SDK Refactor

Split the current `packages/plugin-sdk/` into:

| Package                    | Exports                                            | Used by      |
| -------------------------- | -------------------------------------------------- | ------------ |
| `packages/plugin-sdk/`     | `BuzzPluginAPI` (data + actions + ui interface)    | all surfaces |
| `packages/plugin-sdk-ink/` | `InkUIAdapter` — implements `api.ui` for Ink       | TUI          |
| `packages/plugin-sdk-dom/` | `DOMUIAdapter` — implements `api.ui` for React DOM | Web, Desktop |

Plugin manifest gains an optional `"surfaces"` field:

```json
{
  "entry": "index.ts",
  "inkEntry": "ink.tsx",
  "domEntry": "dom.tsx"
}
```

- `entry` — logic-only activate (registers commands, fetches data)
- `inkEntry` — Ink pane components (used by TUI)
- `domEntry` — DOM React pane components (used by web/desktop)
- A plugin can ship one or both; surfaces ignore entries they don't support

### 4. Browser GUI (`apps/web/`)

**Stack:** Vite + React 19 + packages/ui + packages/theme (DOM adapter)

**Two connection modes:**

| Mode               | How it works                                                           | Key management                                       |
| ------------------ | ---------------------------------------------------------------------- | ---------------------------------------------------- |
| **Local**          | WebSocket to a thin bridge that wraps buzz-sessiond stdio              | OS keychain (same as TUI)                            |
| **Browser-native** | `buzz-session-wasm` compiled via wasm-pack, connects directly to relay | Browser (Web Crypto + IndexedDB or NIP-07 extension) |

**Local mode bridge:** A small Node/Bun HTTP+WS server (`packages/daemon-bridge/`) that spawns buzz-sessiond, translates stdio JSON-RPC to WebSocket JSON-RPC. This is trivial since the protocol is already newline-delimited JSON.

**Browser-native mode:** Compile a subset of `buzz-session` to WASM:

- `RelaySession` (WebSocket connection, NIP-42 auth)
- `Store` (message indexing)
- Key signing (nostr crate's schnorr)
- Expose the same 16 RPC methods as a JS API

Fallback: if WASM compilation of the full session is too complex initially, use [nostr-tools](https://github.com/nbd-wtf/nostr-tools) (JS) with the same method signatures.

### 5. Tauri Desktop App (`apps/desktop/`)

**Stack:** Tauri 2 + same React frontend as `apps/web/`

**Key advantage:** Tauri's backend is Rust, so embed `buzz-session` directly — no subprocess, no WASM, no bridge. The Tauri command handlers map 1:1 to the existing 16 RPC methods:

```rust
#[tauri::command]
async fn channel_list(state: State<'_, SessionState>) -> Result<Vec<Channel>, Error> {
    state.session.channel_list().await
}
```

**Benefits over Electron:**

- ~5MB binary vs ~150MB (no bundled Chromium)
- Native Rust session integration (no IPC overhead)
- System webview (WebKit on macOS, WebView2 on Windows, WebKitGTK on Linux)
- Security: keys never cross a process boundary

**Tauri plugins to use:**

- `tauri-plugin-os` — platform detection
- `tauri-plugin-notification` — native notifications (replace current `osascript` hack)
- `tauri-plugin-store` — config persistence

### 6. Customizability Parity

| Feature        | TUI                      | Browser                   | Desktop                   |
| -------------- | ------------------------ | ------------------------- | ------------------------- |
| Theme tokens   | theme.json -> Ink colors | theme.json -> CSS vars    | theme.json -> CSS vars    |
| Keymap         | keymap.json (once wired) | keymap.json (same parser) | keymap.json (same parser) |
| Plugins (data) | shared plugin-sdk        | shared plugin-sdk         | shared plugin-sdk         |
| Plugins (UI)   | Ink components           | DOM React components      | DOM React components      |
| Slash commands | shared                   | shared                    | shared                    |
| Layout         | fixed (TUI constraint)   | resizable panes, drag     | resizable panes, drag     |

The web and desktop surfaces share the same DOM React components, so any customization that works in one works in the other. The TUI remains limited by terminal constraints but shares the same data and config layer.

---

## Implementation Phases

### Phase 1: Foundation (shared packages extraction)

- Extract theme system into `packages/theme/` with TUI + DOM adapters
- Refactor `packages/plugin-sdk/` to separate data API from UI rendering
- Create `packages/plugin-sdk-dom/` with a DOM UI adapter

### Phase 2: Web GUI

- Scaffold `apps/web/` with Vite + React
- Build `packages/ui/` DOM components (port from Ink shell logic, DOM rendering)
- Implement daemon-bridge for local mode (`packages/daemon-bridge/`)
- Wire connection, channel list, transcript, composer as MVP

### Phase 3: Tauri Desktop

- Scaffold `apps/desktop/` with Tauri 2
- Write Tauri command handlers wrapping `buzz-session`
- Mount the same `packages/ui/` React app in the Tauri webview
- Add native notification support, auto-update, menu bar

### Phase 4: Browser-Native Mode

- Create `crates/buzz-session-wasm/` (wasm-pack)
- Implement key management (Web Crypto + IndexedDB, NIP-07 support)
- Add connection mode toggle to `apps/web/`

### Phase 5: Plugin Ecosystem

- Add `domEntry` support to plugin loader for web/desktop
- Port example plugins (git-status, scratch-notes) to dual-surface
- Document multi-surface plugin authoring

---

## Key Technical Decisions

1. **Single React DOM codebase for web + desktop** — Tauri renders the same app, differing only in the backend transport (Tauri IPC vs WebSocket)
2. **Transport abstraction** — `packages/protocol/` gains a `SessionTransport` interface with implementations for stdio, WebSocket, Tauri IPC, and WASM-direct
3. **Progressive enhancement** — Browser-native mode (Phase 4) is additive; the web GUI ships first with local-daemon-required mode
4. **No Electron** — Tauri provides a better fit given the existing Rust codebase
