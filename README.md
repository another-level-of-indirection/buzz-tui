# buzz-tui

A terminal client for a Buzz relay.

## Setup

Nothing here lives in your shell. The key goes in the OS keychain and the
community list goes in a config file, so a new tab, a reboot, or a different
terminal all behave the same. That is deliberate: an exported variable is gone
the moment you open a new tab, and nothing about a missing one looks like a
mistake — the app just quietly has no key, or fewer communities than you have.

### 1. Install

On a machine with Rust already:

```sh
cargo install --git https://github.com/ianborders/buzz-tui
```

From scratch, including the toolchain:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust 1.88+
. "$HOME/.cargo/env"
cargo install --git https://github.com/ianborders/buzz-tui
```

`cargo install` puts the binary in `~/.cargo/bin`, which rustup adds to your
`PATH`. Building needs a C toolchain for the TLS and keychain crates —
`xcode-select --install` on macOS, `build-essential pkg-config libssl-dev
libdbus-1-dev` on Debian or Ubuntu.

Rust 1.88 is the declared minimum and is checked against, not assumed.

On Linux the key needs somewhere to live: a running Secret Service, which
GNOME Keyring or KWallet provides. Without one, fall back to
`BUZZ_PRIVATE_KEY`. Opening links uses `xdg-open`.

Or from a clone: `cargo install --path .`

### 2. Give it a key

If Buzz Desktop is signed in on this machine, copy the key it already has:

```sh
buzz-tui --keychain-import-from-desktop
```

One macOS prompt, then the two are independent. This is a one-time migration,
not a runtime dependency — reading Desktop's store on every launch would work
against how that store is designed and would break anywhere Desktop is not
installed.

Otherwise, pipe a key in. Reading stdin when it is not a terminal keeps the key
off the screen and out of shell history:

```sh
printf '%s' "$YOUR_NSEC_OR_HEX" | buzz-tui --keychain-import
```

Run it with no pipe and it prompts instead, hidden.

| | |
| --- | --- |
| `--keychain-import` | store a key (stdin or hidden prompt) |
| `--keychain-import-from-desktop` | copy the key Buzz Desktop holds |
| `--keychain-delete` | remove it |
| `BUZZ_KEYCHAIN_ACCOUNT` | keep several identities side by side |

`BUZZ_PRIVATE_KEY` (hex or nsec) still works and matches `buzz-cli`, but it is
the fallback: an exported secret shows up in `ps` output on most systems and is
inherited by every child process the shell spawns.

### 3. Add communities

```sh
buzz-tui --add-community https://your-community.example.com
buzz-tui --communities        # what is configured
```

```sh
buzz-tui --name-community <url> Kybernesis   # what the sidebar calls it
buzz-tui --name-community <url> ""           # back to the default
buzz-tui --remove-community <url>
```

Naming is manual because there is nothing to read: every Buzz deployment
answers NIP-11 with the same generic `"Buzz Relay"`, so a community's name is
something the client decides, exactly as it is in Buzz Desktop. Left unset, the
first label of the relay host stands in — a guess, but a serviceable one.

`BUZZ_RELAY_URL` accepts a comma-separated list and still leads the ordering
when set, which is useful as a one-off override.

### 4. Run it

```sh
buzz-tui
```

### Where things live

| | |
| --- | --- |
| the key | login keychain, service `buzz-tui` |
| community list | `~/.config/buzz-tui/config.json` (macOS: `~/Library/Application Support/buzz-tui/`) |
| read-state identity | `identity.json`, beside the config |
| canvas drafts | `$TMPDIR/buzz-canvas-<channel>.md` |

On macOS a keychain item is ACL-bound to the binary that created it, so
rebuilding and reinstalling `buzz-tui` can produce one approval prompt on the
next launch. "Always Allow" quiets it until the next rebuild. That is a
property of unsigned local builds, not something a client can avoid.

### When something looks wrong

```sh
buzz-tui --probe
```

Connects, authenticates, and walks the same startup the app does — reporting
where the key came from, which communities are configured, and per channel how
many messages came back and how many were top-level. A blank channel has
several causes that are indistinguishable from inside a full-screen app: a bad
key, a host that maps to no community, a pubkey that is a member of nothing, a
query that returned nothing, or a channel that is genuinely all thread replies.
This separates them, with timings.

## Keys

`F1` or `Ctrl-H` opens the full reference in the app. `Ctrl-H` is bound only
where the terminal implements the keyboard protocol — everywhere else `Ctrl-H`
and Backspace are the same byte, so binding it would swallow Backspace and
break the composer. `F1` works everywhere, and the hint strip names whichever
one your terminal can deliver.

| Input | Action |
| --- | --- |
| `Tab` / `Shift-Tab` | next / previous channel |
| `Ctrl-K` | switch community |
| `@` | mention autocomplete |
| `Enter` | send, or accept a completion |
| `Shift-Enter` / `Alt-Enter` / `Ctrl-J` | newline |
| `Ctrl-W` / `Ctrl-U` | delete the last word / clear the draft |
| `Ctrl-F` | search every channel |
| `Ctrl-T` | open the most recent thread |
| `Ctrl-E` | react to the newest message — or, in the canvas, edit it |
| `Ctrl-G` | the channel's shared canvas |
| `Ctrl-N` | start a conversation with anyone |
| `Ctrl-X` / `Ctrl-R` | hide or restore a DM / reveal hidden DMs |
| `PgUp` / `PgDn` | scroll a page |
| `Esc` | leave a thread, canvas, popup, or search |
| `Ctrl-C` | quit |

The mouse does real work and the help popup lists it, which a hint strip
cannot: click a channel or community to open it, a name to react to that
message, a reaction to add or take yours back, `N replies` to open that thread,
and the wheel scrolls whichever pane the pointer is over.

Mouse reporting is on while the app runs, which means the terminal's own
click-drag text selection is suppressed. Hold `Option` (macOS) or `Shift`
(most Linux terminals) to select text the usual way.

## Communities

A community *is* its relay URL — the relay resolves which one a request belongs
to from the host, before authentication, and fails closed on a host it does not
recognise. There is no server-side "list my communities" to ask for; the desktop
client keeps its own list too.

Each gets its own live socket, store and read-state — that concurrency is the
whole reason to have them listed, and it is what a switch-and-reconnect design
could not give.

They appear as a third list in the sidebar, above `channels` and `direct` —
the order they contain each other in. Click one to switch, or press `Ctrl-K`.
A list you can see makes switching discoverable without a hint telling you it
exists; a key alone does not. With one community the panel is not drawn, since
a box listing a single entry spends chrome to say "you are here".

Each row carries its unread and a `●` if any of it mentions you, and a
community that is not connected renders dimmed — worth seeing before you wonder
why it has nothing in it.

Parallel sessions do not compete for the relay's frame budget: admission is
keyed per `(community, pubkey)`, so each community has its own.

## Look

```text
 ╭ channels ───────╮╭ #general ─── relay ● live ╮╭ ↩ thread ── esc ✕ ╮
 │                 ││                           ││                   │
 │   dev           ││   Ian  03:29              ││   Ian  03:29      │
 │ ▌ general    3  ││   @samantha hi from the   ││   @samantha hi    │
 │   welcome       ││   tui                     ││   from the tui    │
 ╰─────────────────╯│   ▌ 1 reply               ││                   │
 ╭ direct ─────────╮│                           ││   Samantha  03:29 │
 │                 ││   Oxnfrith  03:21         ││   Hey Ian — hi    │
 │   Kyber         ││   test                    ││   back from the   │
 │   Fizz       1  ││                           ││   other side.     │
 ╰─────────────────╯╰───────────────────────────╯╰───────────────────╯
 ╭ compose ─ ⇥ channels  @ mention  esc close thread  ⏎ send ────────╮
 │                                                                   │
 │   › Reply in thread                                               │
 │                                                                   │
 ╰───────────────────────────────────────────────── caught up ───────╯
```

Every pane is a rounded box with interior padding, and the whole frame sits
inside a one-cell margin so nothing hugs the terminal edge. A border earns its
two columns by carrying something: the pane's name, the relay and connection
state, the key bindings, and the latest notice all live on borders rather than
spending body rows on a status strip. Bindings sit on the composer's *top*
border and never move — they are the only way to discover the app, so transient
state goes on the bottom border instead of displacing them.

Two accents carry meaning and nothing else borrows them. **Cyan** is structure
and focus — the selected channel, the compose caret, the channel name.
**Emerald** is liveness and self — a healthy connection, today's date separator,
and your own name in the transcript. Author colors deliberately avoid both, so
nobody's name reads as "selected" or as "you". Everything else is one of four
neutrals, which is what keeps two accents legible.

Rooms and direct messages are separate panes rather than one list with headings
inside it. A workspace routinely has more DMs than rooms, all named after people
rather than topics, so an undivided list buries the rooms — and a heading drawn
in the body competes with the pane titles instead of matching them. When a
workspace has only one kind, only one pane is drawn: an empty box titled
"direct" is worse than no box.

The selected row's highlight spans the full width: a background that stops at
the end of a word reads as a selected *word* rather than a selected row.

Consecutive messages from one author inside five minutes share a header, the way
every desktop chat client groups them. The cursor is a bar rather than a block:
a block inverts the cell it sits on, so parked in an empty composer it blacks
out the first letter of the placeholder.

## The loader

Waiting states carry the braille "columns" loader ported from Kyber Studio's
`Spinner.tsx` — six columns filling bottom-up, then a flash of full and empty,
26 frames at 60ms. Frames are generated rather than listed, so the sequence
cannot drift out of order the way a hand-typed frame list does.

Braille suits a terminal better than a browser: a braille cell *is* a terminal
cell, two dots wide by four tall, so the animation renders at native resolution
instead of being approximated. The frame is a pure function of elapsed time, so
nothing owns the animation, nothing can forget to advance it, and two spinners
on screen stay in phase.

It appears while messages or channels load, beside `connecting` (a connection
state that does not move is indistinguishable from one that has hung), and
beside the typing indicator — which is what an agent working looks like. The
app repaints at 60ms only while something is animating and drops back to once a
second when nothing is; repainting sixteen times a second forever would be a
rude thing for a chat client to do to a laptop.

## Typing indicators

`Samantha is typing…` rides the composer's bottom border, opposite the notice.
It costs no rows — a line appearing and disappearing inside the transcript
would shove the conversation up and down while someone is mid-sentence.

Kind 20002 is ephemeral and WebSocket-only, so it rides the focused channel's
live subscription and never touches a history query. Constants match the
desktop client so the two agree about who is typing: republished every 3s while
composing, expiring 8s after the last one, and suppressed for 2s after that
person's message lands — their final indicator routinely arrives just *after*
the message it preceded, and without that they flicker back to typing.

Because it expires on a clock rather than on an event, the app repaints once a
second. Agents emit these while they work, so this is also what "the agent is
thinking" looks like.

## Composing

The composer grows with the draft, to ten rows, then scrolls internally so the
caret stays on screen. Hard newlines and wrapping are both honoured, and blank
lines survive — Markdown leans on them, and collapsing one turns a formatted
message into a wall of text at the moment it is sent.

Newline is bound three ways because no single one works everywhere. Shift+Enter
needs the terminal keyboard protocol, which this client enables when the
terminal advertises support; without it, Shift+Enter and Enter are the same
byte and a binding would silently do nothing. Alt+Enter arrives ESC-prefixed
and Ctrl-J is a distinct byte from Ctrl-M, so both work anywhere. The hint
names whichever one this terminal can actually deliver.

Bracketed paste is enabled, which is what keeps a pasted block multi-line —
without it the terminal delivers the newlines as Enter keypresses and the
message sends one line at a time.

## Direct messages

Buzz has no DM delete — hiding is the only way to get a stale conversation out
of the sidebar, which matters when a retired agent leaves one behind sharing a
display name with its replacement. `Ctrl-X` hides the selected DM by publishing
`kind:41012`; the relay recomputes a `kind:30622` visibility snapshot listing
every DM you have hidden, and that snapshot is the authority
([NIP-DV](../../docs/nips/NIP-DV.md)). It is queried by `#p`, not `#d` — `p` is
the tag the relay's read-authorization gate checks.

Hidden DMs are announced rather than silently dropped: the `direct` pane's
border shows how many, `Ctrl-R` reveals them dimmed, and `Ctrl-X` on a revealed
one restores it. Only DMs are ever hidden; the spec is explicit that non-DM
channels must not be affected, and there is a test for it.

`Ctrl-N` opens a conversation with anyone whose profile has loaded, whether or
not a DM already exists. Restoring and starting are the same operation: the
relay's `open_dm` dedupes on the participant set, so it returns the existing
conversation and clears `hidden_at` rather than creating a second one.

## History

Scrolling near the top of a channel loads the page before it. The cursor is
`until: <oldest loaded>`, which is *inclusive* in NIP-01 — so each page
re-delivers the boundary second and the store drops the duplicates. That
overlap is deliberate: asking for strictly-older would silently drop every
message sharing a second with the boundary, and a busy channel has several per
second. The bridge's `before_id` cursor solves this exactly, but it is a
bridge-only filter extension and this client speaks WebSocket.

When the relay stops returning anything new, the top of the transcript reads
*"No earlier messages loaded"* rather than claiming the channel began there.
Over WebSocket there is no authoritative exhaustion signal — `kind:39006`
`has_more` is bridge-only — so the client reports what it observed instead of
making a claim it cannot support.

Opening a search hit also loads the neighbourhood around it. Without that, a
hit from months ago is one message sitting between two unrelated days:
chronologically correct and completely unreadable.

## Canvas

`Ctrl-G` opens the channel's shared Markdown document — the scratchpad agents
keep architecture notes, runbooks and status boards in. It replaces the
transcript rather than splitting with it: a document deserves the width, and
the conversation is one keystroke away. `Ctrl-E` opens it in `$VISUAL`, then `$EDITOR`, and with neither set it prefers
`nano` over `vi` — dropping someone who never asked for an editor into a modal
one with no on-screen way out is a trap, and `nano` prints its own key hints.
The empty-canvas placeholder names whichever editor will open, so it is not a
surprise. Handing over means releasing the terminal completely: raw mode, the
alternate screen, mouse reporting and the cursor style all have to go and come
back.

Kind 40100 is **not** replaceable, so every save is a new event and the relay
keeps them all. The newest wins, which also means the newest silently
clobbers: there is no compare-and-swap, so two people saving a minute apart
lose one of the edits. This client records the revision it loaded from and
refuses to publish over a newer one, keeping your draft at
`$TMPDIR/buzz-canvas-<channel>.md` — refusing costs nothing, and clobbering
someone's work is not recoverable.

The byline names who last changed it and when, because on a shared document
that is what tells you whether to trust it.

## Search

`Ctrl-F` searches every channel you can read, over the relay's Postgres
full-text index (NIP-50). Enter runs the query; once results are up, Enter opens
the highlighted hit — one key for both, because they are never both meaningful
at once. A hit that is a thread reply opens its thread, since landing on the
channel and leaving you to find it would be no answer at all.

Results are folded into the store like any other event, which is what makes a
hit from months ago insert at its chronological place and become reachable by
scrolling. The message is then placed a third of the way down the pane rather
than at the very top, so what came before it is visible as context.

The relay rejects a REQ that mixes search and non-search filters outright, so
the query goes out on its own — which this session does anyway, one filter per
REQ.

## Reactions

Reactions render as pills under the message they belong to, collapsed by emoji
in first-seen order — sorting by count would make the row reorder itself as
people react, which is hard to click and harder to read. Your own is cyan, and
clicking it takes it back.

Adding one: click a message's name to open the picker for it, or `Ctrl-E` for
the newest message. There is no message-selection cursor, so the header row is
the handle.

Two details the wire format forces. `+` and `-` are NIP-25's *like* and
*dislike* rather than literal characters, so they render as 👍 and 👎 — a client
that printed them shows a lone plus where every other client shows a thumb. And
`build_reaction` carries only the `e` target; the relay derives the channel
from it for storage, but live fan-out is topic-based, so this client adds the
`h` tag or nobody sees the reaction until they refetch.

Reactions are applied locally before publishing. The event is signed first, so
the optimistic copy carries the same id the relay echoes back and the dedupe
makes the echo a no-op — no phantom, no double count.

Custom workspace emoji (`kind:30030`) are not offered in the picker: a terminal
cannot render the image a shortcode points at. Incoming ones show as bare
`:shortcode:` text, which is the honest limit.

## Read state and mentions

Unread is derived from a cross-device read frontier, not counted locally.
Following [NIP-RS](../../docs/nips/NIP-RS.md), read position lives in
`kind:30078` events the user publishes to themselves, NIP-44 encrypted to their
own key, and the effective frontier is the componentwise `max()` across every
coordinate. Reading a channel in Buzz Desktop advances it there and this client
picks it up — without that, badges here disagree with the desktop and stop
meaning anything.

Frontier entries only. The spec's manual-unread override layer (`ov_*` keys) is
deliberately absent: it is durable state with its own eviction and
full-state-load obligations, and this client has no "mark as unread" feature to
justify them. A client that neither reads nor writes `ov_*` may narrow its fetch
by tag and age, which is what this does.

A message that `p`-tags you gets a cyan rail down its left edge, and its channel
gets a `●` in the sidebar beside the count. "Someone spoke" and "someone spoke
to you" are different facts, and one number cannot carry both.

## Threads

The channel timeline shows top-level messages only. That is the relay's own
frozen contract — *"replies never enter the channel timeline"* — and rendering
them flat is what makes almost every message look like a thread reply. A
message with replies carries a `↳ N replies` affordance; clicking it opens the
thread in place, and `Esc` returns.

While a thread is open the composer replies into it, and says so: the pane is
titled `reply` and the placeholder reads "Reply in thread". The same keystroke
sends to two different places depending on a mode the reader may have
forgotten they are in, so it is stated rather than implied.

Threads are reassembled client-side from NIP-10 tags, which is what the channel
window spec expects of a generic Nostr client. The relay also offers an
authoritative top-level view with server-computed reply counts (`top_level`,
`kind:39005` summaries) over the HTTP bridge — not the WebSocket path this
client uses. Counts here are exact for the loaded window and no wider.

## Links and images

Links are clickable — the renderer records which columns each one occupies and
the app resolves the click itself, rather than emitting OSC 8 hyperlinks.
Ratatui accounts for width per cell, so escape sequences smuggled into a span
corrupt its layout; and OSC 8 is unsupported in Terminal.app anyway. Doing the
hit-testing here works in every terminal.

A link that wraps becomes one target per line: a terminal has no notion of a
shape spanning rows, so each fragment is clickable on its own.

Bare URLs are found too. `pulldown-cmark` only autolinks the CommonMark
`<url>` form — `ENABLE_GFM` covers blockquote alerts, not autolink literals —
so this scans text runs itself. Two details that took a second attempt:
trailing sentence punctuation is excluded (`see https://example.com.` means the
site, not a path ending in a dot) while a genuinely-contained bracket survives;
and text is buffered before scanning, because the parser splits a run at
anything that *might* be an emphasis delimiter, so a URL containing an
underscore arrives in pieces.

Images render as `🖼 alt text`, clickable, opening in whatever handles the URL.
There is no attempt to draw them: the kitty and iTerm2 graphics protocols are
terminal-specific, and a client that showed a picture on one machine and a
broken box on the next is worse than one that consistently hands it to
something that can.

Only `http` and `https` are opened. A Markdown link can carry any scheme, and
handing `file://` or a custom scheme to the system opener on someone else's
say-so is a way to run something the reader did not choose.

## Markdown

Messages render as GitHub-flavored Markdown: emphasis, inline code, fenced
blocks framed and labelled with their language, headings, nested lists, block
quotes, links, and aligned tables. Agents write formatted messages, so a client
that shows the asterisks is showing the reader the wrong thing.

Wrapping happens inside the renderer rather than in ratatui's `Paragraph`,
because it has to survive styling — a bold run crossing a line break must stay
bold on both lines, which wrapping finished spans cannot do.

## Mentions

Typing `@` opens an autocomplete over the current channel's roster. Only members
with a real profile name are offered: mention resolution matches names against
kind-0 profiles, so completing to a hex fallback would insert an `@` that
silently fails to tag anyone. The query runs to the end of the input rather than
to the next space, so a two-word display name can be completed from `@Fizz B`.

Buzz carries mentions as `p` tags, not as text. An agent harness subscribes
with `#p` set to its own key, so a message that names an agent only in the body
never reaches it — it looks ignored rather than undelivered.

So `@name` in the composer is resolved against the channel roster before
sending, and a DM addresses every other participant whether or not the text
contains an `@`. That second rule is the desktop's, in
`messageMentionPubkeys.ts`; without it an agent never answers a DM.

## What it does

Channels and direct messages across several communities at once; threads in a
side pane; GitHub-flavored Markdown; `@` mentions with autocomplete; reactions;
full-text search; the channel canvas; typing indicators; unread that syncs with
Buzz Desktop through NIP-RS; history that pages backwards; and a composer that
grows. The mouse works throughout.

Out of scope, and likely to stay there: voice huddles, workflows, the git and
projects surface, media upload, agent management, and moderation. The
interesting terminal client is the one that does the things you actually do in
a terminal and refuses the rest — chasing parity with a desktop app whose
agents feature alone is 43k lines of TypeScript is not a goal.

## Relation to block/buzz

This is a client, not a fork. It depends on three crates from
[block/buzz](https://github.com/block/buzz) — `buzz-core` for the kind
registry, `buzz-sdk` for typed event builders, and `buzz-ws-client` for the
NIP-01 wire format and NIP-42 handshake — pinned to a revision rather than a
branch. That project moves fast enough to have merged PR #6359 the day this
client was started, and an unpinned dependency would mean the build breaking on
someone else's schedule.

To take newer upstream crates, change the three `rev` values in `Cargo.toml`
together and run the tests. They should move as a set: `buzz-sdk` depends on
`buzz-core`, and mixing revisions asks cargo to resolve two versions of the
same crate.

`src/session.rs` is a port of `native_relay_client.rs` from that project — see
NOTICE. Extracting that module upstream into a crate both Buzz Desktop and this
client depend on would delete the file; it is the change most worth
contributing back.

## Layout

| File | Responsibility |
| --- | --- |
| `session.rs` | One authenticated socket, many multiplexed subscriptions. Reconnect, resubscribe, and CLOSED recovery. |
| `store.rs` | A pure fold from events to state. No socket, no terminal. |
| `app.rs` | State plus the actions that change it. All relay work is spawned. |
| `ui.rs` | Reads `App`, writes a frame. |

Three rules hold the design together:

1. **The socket task never renders.** The relay pushes with `try_send` and
   drops a connection after three consecutive full buffers, so a slow paint on
   the read loop is a disconnect. Events leave the session through a channel.
2. **A subscription id's filter is immutable for the life of the session.** To
   change a filter, use a new id — which is why `app.rs` derives unread
   subscription ids from a fingerprint of the channel set they watch.
3. **Ordering is `(created_at, id)`, never `created_at` alone.** Events sharing
   a second are common, and a timestamp-only sort makes their order depend on
   arrival, so a backfilled page and a live delivery disagree.

`session.rs` is a close port of
`desktop/src-tauri/src/native_relay_client.rs`, which solved the same problem
for the desktop backend but lives in a crate the workspace excludes. Extracting
that module into a shared crate both clients depend on would delete this file.
