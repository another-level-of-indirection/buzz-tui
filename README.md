# buzz-tui

A terminal client for [Buzz](https://buzz.xyz) relays — hybrid architecture
with a Rust session daemon and a TypeScript (Bun + Ink) shell.

Forked from [ianborders/buzz-tui](https://github.com/ianborders/buzz-tui).
Canonical fork: [another-level-of-indirection/buzz-tui](https://github.com/another-level-of-indirection/buzz-tui).
The original ratatui client is preserved in `src/` as a parity checklist.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  TypeScript shell (Bun + Ink)                       │
│  layout · themes · keymap · panes · plugin host     │
└───────────────────────┬─────────────────────────────┘
                        │ JSON-RPC 2.0 (stdio)
┌───────────────────────▼─────────────────────────────┐
│  Rust session daemon (buzz-sessiond)                │
│  RelaySession · Store · identity/keychain · NIP-RS  │
│  pins buzz-sdk / buzz-core / buzz-ws-client         │
└───────────────────────┬─────────────────────────────┘
                        │ NIP-29 / NIP-42 WebSocket
                        ▼
                   Buzz relay
```

Private key material never leaves the Rust process. The TypeScript shell
receives display names, pubkeys, and UI-ready projections — never raw events
or secrets.

## Setup

### Prerequisites

- **Rust 1.88+** — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Bun** — `curl -fsSL https://bun.sh/install | bash`
- On macOS: `xcode-select --install`
- On Linux: `build-essential pkg-config libssl-dev libdbus-1-dev`

### Install

```sh
./install.sh
```

This builds `buzz-sessiond` (release), installs JS dependencies, and creates
a `buzz-tui` launcher in `~/.local/bin/`. Or manually:

```sh
cargo build -p buzz-sessiond --release
bun install
```

### Give it a key

If Buzz Desktop is signed in on this machine, copy the key it already has:

```sh
buzz-tui --keychain-import-from-desktop
```

Otherwise, pipe a key in:

```sh
printf '%s' "$YOUR_NSEC_OR_HEX" | buzz-tui --keychain-import
```

| Flag | Action |
| --- | --- |
| `--keychain-import` | store a key (stdin or hidden prompt) |
| `--keychain-import-from-desktop` | copy the key Buzz Desktop holds |
| `--keychain-delete` | remove it |
| `BUZZ_KEYCHAIN_ACCOUNT` | keep several identities side by side |

`BUZZ_PRIVATE_KEY` (hex or nsec) still works as a fallback.

### Add communities

```sh
buzz-tui --add-community https://your-community.example.com
buzz-tui --communities
```

Or set `BUZZ_RELAY_URL` (comma-separated for multiple).

### Run

```sh
buzz-tui
```

For development:

```sh
./dev.sh   # builds daemon + launches shell
```

## Keys

| Input | Action |
| --- | --- |
| `Tab` / `Shift-Tab` | next / previous channel |
| `Enter` | send message (or execute `/slash` command) |
| `Ctrl-J` | select message (enter action mode) |
| `Ctrl-T` | open the most recent thread |
| `Ctrl-R` | reply in thread (to selected message) |
| `Ctrl-F` | search (NIP-50) |
| `Ctrl-G` | toggle channel canvas |
| `Ctrl-E` | edit canvas (when canvas is open) |
| `Ctrl-S` | save canvas edit |
| `Ctrl-M` | toggle member pane |
| `Page Up` | load older history |
| `↑` / `↓` | select message in thread or action mode |
| `Esc` | close thread / canvas / search / member pane |
| `Ctrl-C` | quit |

### Message action mode (`Ctrl-J`)

| Key | Action |
| --- | --- |
| `t` | open thread on selected message |
| `r` | reply to selected message |
| `e` | react with +1 |
| `d` | delete own message |
| `Esc` | cancel |

### Slash commands

| Command | Action |
| --- | --- |
| `/help` | list available commands |
| `/search <query>` | search messages |
| `/canvas` | toggle canvas pane |
| `/channels` | list channels |

Plugin commands are merged into the same namespace.

## Themes

Override any color token in `~/.config/buzz-tui/theme.json`:

```json
{
  "authorName": "#ff9900",
  "channelSelected": "magenta",
  "threadBorder": "#00ff88",
  "border": "#333333"
}
```

40+ tokens cover every visual element: chrome, channels, transcript,
threads, canvas, search, members, and composer. See
[`src/theme.ts`](packages/shell/src/theme.ts) for the full list and defaults.

## Keymap

Override key bindings in `~/.config/buzz-tui/keymap.json`:

```json
[
  { "combo": { "key": "k", "ctrl": true }, "action": "channel_prev" },
  { "combo": { "key": "j", "ctrl": true }, "action": "channel_next" }
]
```

Overrides are checked before defaults, so you can remap without removing
built-in bindings.

## Plugins

Plugins add custom panes, slash commands, and keybindings without touching
Rust. See [PLUGIN_GUIDE.md](PLUGIN_GUIDE.md) for the full authoring guide.

### Included plugins

- **git-status** — shows `git status --short` in a side pane (`/git`)
- **scratch-notes** — local notepad persisted to disk (`/notes`, `/note <text>`)

### Plugin API

Plugins get scoped access — never private keys:

- **Read:** `channelList()`, `storeSnapshot()`, `storeThread()`, `canvasGet()`, `channelSearch()`
- **Write:** `messageSend()`, `messageReply()`, `messageReact()`
- **UI:** `setPane()`, `registerCommand()`, `showNotice()`

## Repo layout

```
buzz-tui/
  crates/buzz-session/      # Rust library: session, store, identity, readstate
  crates/buzz-sessiond/     # Rust binary: JSON-RPC daemon (16 methods, 7 notifications)
  packages/protocol/        # shared Zod schemas for the RPC bridge
  packages/shell/            # TypeScript Ink app (8 components)
  packages/plugin-sdk/       # plugin lifecycle API
  plugins/git-status/        # example plugin
  plugins/scratch-notes/     # example plugin
  src/                       # original ratatui TUI (parity checklist)
```

### Daemon RPC methods

| Method | Description |
| --- | --- |
| `identity.status` | pubkey and connected communities |
| `community.list` | list communities with display names |
| `channel.list` | channels with unread counts and mention badges |
| `channel.focus` | subscribe to a channel's live events |
| `channel.history` | load older messages |
| `channel.search` | NIP-50 full-text search |
| `message.send` | send a message with @mention resolution |
| `message.reply` | reply in a thread |
| `message.react` | add a reaction |
| `message.delete` | kind:5 deletion |
| `typing.set` | emit typing indicator |
| `canvas.get` | get channel canvas |
| `canvas.set` | save canvas with refuse-to-clobber |
| `store.snapshot` | messages + typing for a channel |
| `store.thread` | threaded messages for a root event |
| `store.members` | channel participants with display names |

### Daemon push notifications

`session.ready`, `session.connected`, `session.disconnected`,
`store.event`, `store.eose`, `store.channels_loaded`, `session.notice`

## Relation to block/buzz

This is a client, not a fork. It depends on three crates from
[block/buzz](https://github.com/block/buzz) — `buzz-core` for the kind
registry, `buzz-sdk` for typed event builders, and `buzz-ws-client` for the
NIP-01 wire format and NIP-42 handshake — pinned to a revision rather than a
branch.

To take newer upstream crates, change the three `rev` values in the workspace
`Cargo.toml` files together and run the tests.

## Where things live

| | |
| --- | --- |
| the key | login keychain, service `buzz-tui` |
| community list | `~/.config/buzz-tui/config.json` (macOS: `~/Library/Application Support/buzz-tui/`) |
| theme | `~/.config/buzz-tui/theme.json` |
| keymap | `~/.config/buzz-tui/keymap.json` |
| scratch notes | `~/.config/buzz-tui/scratch-notes.txt` |
| read-state identity | `identity.json`, beside the config |
