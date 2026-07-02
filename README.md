# zmux

A terminal multiplexer with pane-local mouse wheel scrolling, an in-daemon MCP server, and agent prompt shims. Started as a tmux/screen alternative, grew an agent-harness layer for working with AI coding agents in panes.

## What's in the box

**Core multiplexer:**
- bounded scrollback ring per pane, viewport that detaches from the live bottom on wheel events
- pane-local mouse wheel routing, with translated SGR pass-through for alt-screen apps
- VT ingester covering alternate-screen, cursor save/restore (position + SGR pen, including the implicit DECSC/DECRC on 1049 alt-screen switches), scroll regions, line/char editing, mouse modes, Synchronized Output (mode 2026), REP, colon-SGR subparameters (kitty underline styles, colon-form extended colors, underline-color consumption), OSC 10/11 color queries with terminator round-trip, DECSCUSR, bracketed paste (2004), focus events (1004), and a wide-char width table audited for emoji / CJK / box drawing
- detached per-session daemon, named-session attach/reattach, multi-window workspace
- Ctrl-a-prefix bindings for splits, resize, focus cycling, layout presets, OSC 52 yank, scrollback search, paste, and more

**Agent harness:**
- structured pane observability — every pane has an `AgentState` (Idle / Working / AwaitingInput / Errored / Exited) derived from PTY behavior with no agent cooperation required, plus `last_command` and `last_exit` capture and an EventBus that broadcasts lifecycle events
- supervisor overlay (`Ctrl-a A`) — live dashboard of every pane in the session with status glyphs, filtering, in-overlay rename, confirm-then-kill, and broadcast-input-to-all
- in-daemon MCP server — exposes pane control as MCP tools (`list_panes`, `spawn_pane`, `send_keys`, `wait_pane`, `read_pane`, `read_pane_output`, `kill_pane`, `set_label`, `watch_events`) and a `zmux://panes` resource so external AI clients (Claude Code, Cursor) can spawn agents in panes, watch their progress, and intervene over a Unix socket
- `zmux claude` / `zmux codex` prompt shims that drive interactive agent TUIs in panes with a marker-contract automation protocol, optional worker JSON output, and Claude hook capture for clean result extraction
- VT capture/replay tooling for bisecting rendering bugs against agent CLIs

## Building

Requires **Rust 1.85+** (the project is on the 2024 edition). `cargo build --release` produces a single binary at `target/release/zmux` — copy it onto your `$PATH` (e.g. `~/.local/bin`) and it's ready.

## Running it

### Single-process modes (mostly for dev/demo)

- `cargo run --quiet` — bounded PTY demo, prints captured lines.
- `cargo run -- --shell` — single-pane interactive shell.
- `cargo run -- --mux` — two-pane foreground workspace.

### Detached sessions

Sessions outlive the attached client. The daemon owns PTYs and workspace state; clients attach over `$TMPDIR/zmux-$USER/<name>.sock`.

```
zmux new [name]            create + attach (default name "default")
zmux attach [name]         reattach to an existing session
zmux ls                    list live sessions (hides stale + .mcp.sock entries)
zmux ls --verbose          ls plus per-pane state, last command, last exit
zmux kill [name]           shut a session down (use --all to kill every live session)
zmux prune [--dry-run]     remove stale session and .mcp.sock files
zmux serve [name]          run server in the foreground (usually called by `new`)
zmux capture <s> <p> <f>   dump raw PTY bytes from session s, pane p, into file f
zmux label <s> <p> <text>  set a human label on a pane (use - to clear)
zmux mcp --session <name>  stdio bridge for MCP clients (Claude Code, Cursor, etc.)
```

### Ctrl-a prefix bindings (while attached)

| Key | Action |
|---|---|
| `d` | detach client (session keeps running) |
| `\|` | split active pane vertically (new shell to the right) |
| `-` | split active pane horizontally (new shell below) |
| `x` | close active pane (last pane is preserved; use `kill`) |
| `o` / `p` | cycle focus next / previous |
| `c` | open a new window |
| `n` / `P` | next / previous window |
| `Ctrl-a` (again) | toggle to the previously active window (screen-style `other`) |
| `a` | send one literal `Ctrl-a` to the pane (readline beginning-of-line etc.) |
| `1-9` | jump to window by index |
| `&` | close active window (only when more than one exists) |
| `{` / `}` | swap active pane with the previous / next pane |
| `s` | session-picker overlay (1-9 select, Esc cancels) |
| `,` | rename active pane |
| `!` / `^` | split running a prompted command (Enter runs, Esc cancels) |
| `z` | zoom (toggle active pane fullscreen) |
| `q` | overlay pane numbers |
| `H` / `J` / `K` / `L` | resize active pane border |
| `2` / `3` / `4` | layout presets (two-col / three-col / four-quad — single-pane only) |
| `Space` | cycle layout presets |
| `y` / `]` | yank visible viewport via OSC 52 / paste last yank |
| `[` | enter scrollback mode |
| `=` | toggle synchronize-panes — mirror every keystroke to every pane in the active window; `[SYNC]` appears in the status bar while on |
| `:` | open the command prompt (see below) |
| **`A`** | **open the supervisor overlay** |

### Supervisor overlay (`Ctrl-a A`)

A live dashboard of every pane in the session — including panes in other windows, which carry a `w2:` tag and stay live in the list as session events stream in. `Enter` on a tagged row switches to that window and focuses the pane; kill, label, and broadcast reach panes in any window.

```
┌─ zmux supervisor [working] ─ 3 of 7 panes ─┐
│ ● claude #1     working   "refactor auth"  │
│ ● claude #2     working   tests passed     │
│ ● ollama dev    working   streaming        │
└── j/k navigate · enter attach · b broadcast · k kill ─┘
```

Status glyphs: `●` working, `○` idle, `⚠` awaiting input, `✗` errored, `◐` exited.

Bindings:

| Key | Action |
|---|---|
| `j` / `k` (or arrows) | move selection |
| `Enter` | attach to selected pane (overlay closes) |
| `q` / `Esc` | close overlay |
| `f` | cycle filter: all → working → idle → awaiting → errored → all |
| `K` | kill selected pane (`y` to confirm; any other key cancels) |
| `l` | edit label of selected pane (Enter commits, Esc cancels) |
| `b` | broadcast input: confirm with `y`, type a line, Enter sends to every working/idle pane in the current filter |

### Scrollback mode (`Ctrl-a [`)

| Key | Action |
|---|---|
| `j` / `k` | line down / up |
| `Ctrl-D` / `Ctrl-U` | half-page down / up |
| `Space` / `b` | full-page down / up |
| `g` / `G` | top / bottom (G follows live output) |
| `/` | search prompt (Enter commits, Esc cancels) |
| `n` / `N` | next / previous match |
| `v` | begin selection (character-by-character) |
| `V` | begin selection (whole-line) |
| `R` | begin selection (rectangle / block) |

### Selection mode (after `v` / `V` / `R`)

Extend the selection from where it started.

| Key | Action |
|---|---|
| `j` / `k` | extend one line down / up |
| `h` / `l` | extend one char left / right |
| `g` / `G` | extend to buffer top / bottom |
| `Space` / `b` | extend full-page down / up |
| `Ctrl-D` / `Ctrl-U` | extend half-page down / up |
| `y` | yank the selection via OSC 52 and exit |
| `v` / `V` / `R` / `Esc` | cancel selection and return to live output |

### Command prompt (`Ctrl-a :`)

A tmux-style command line. Type the command and press Enter; Esc cancels. Registered commands:

| Command | Effect |
|---|---|
| `display-message <text>` | flash the text in the status bar |
| `capture <session> <pane> <path>` | dump raw PTY bytes for a pane to a file (same as the `zmux capture` CLI) |

The prompted-split bindings `Ctrl-a !` (right) and `Ctrl-a ^` (down) accept any shell command line and spawn it in a new pane; `Ctrl-a :` is reserved for the registered zmux commands above.

## Security

**zmux is an agent control plane, not a sandbox.** Anything that can `connect()` to the daemon's MCP socket gets shell-equivalent authority over your terminal sessions: spawn arbitrary commands, type any keys (including `rm -rf /`, `sudo …`, `curl … | sh`), kill panes, and read every byte of pane output and scrollback. The MCP server intentionally does no command filtering, no per-tool capability negotiation, and no client authentication beyond filesystem permissions.

**What protects the socket today:**
- The session directory `$TMPDIR/zmux-$USER/` is created with mode `0o700` (re-applied on every daemon start so an older zmux can't leave it world-readable).
- Each session socket and `.mcp.sock` is `chmod 0o600` immediately after bind.
- The daemon does not spawn from `inetd` / `systemd-socket-activate`-style facilities, so the perms above are the only relevant ACL.

**What does NOT protect the socket:**
- There is no per-connection auth handshake, capability check, or rate limit beyond a 1 MiB JSON-RPC line cap and a 1024-payload outbound queue.
- The supervisor overlay's confirm-then-kill UX is for humans at the terminal — MCP clients bypass it entirely.

**Audit log:** every mutating MCP call (`send_keys`, `spawn_pane`, `kill_pane`, `set_label`) is appended as a JSON line to `$ZMUX_STATE_DIR/audit/<session>.jsonl` (mode `0600`) with a timestamp and a per-connection id, so concurrent controllers can be told apart after the fact. This is forensics, not access control: it records what happened, it does not prevent anything, and connection ids identify sockets, not users. Keystroke payloads are truncated at 2 KiB.

**Operational consequences:**
- Treat any MCP client you wire up the same way you'd treat a logged-in shell on the same machine. The Claude Code config snippet below puts Claude Code in exactly that trust position.
- Do not widen the socket permissions, do not proxy the socket over a network, and do not run the daemon as a different user than the agents/clients it serves.
- The `zmux pair` co-pilot mode is the one exception to "no extra friction" — every `send_keys` it proposes prompts the user for `[y/N]` confirmation in the pair pane. That's a UX-layer gate, not an MCP-layer gate; an unconfirmed pair instance still has the same raw authority over the MCP socket if you change its code.

## MCP server — driving zmux from an AI client

The daemon exposes a JSON-RPC 2.0 MCP server (protocol version `2025-06-18`) on `$TMPDIR/zmux-$USER/<session>.mcp.sock`. Use the stdio bridge for clients that only speak stdio. See [Security](#security) above before wiring up your first client.

### Wiring up Claude Code

Add to your `claude_desktop_config.json` (or equivalent MCP config):

```json
{
  "mcpServers": {
    "zmux": {
      "command": "zmux",
      "args": ["mcp", "--session", "default"]
    }
  }
}
```

Now Claude Code can call:

| Tool | Purpose |
|---|---|
| `list_panes` | list every pane in the session with window index, state, label, last command, last exit, size |
| `spawn_pane` | spawn a new pane running a command (`split`: `"h"`, `"v"`, or `"window"`; window spawns land in the background and do not steal the attached client's view) |
| `send_keys` | type into a pane (`enter: true` presses Enter; `clear_input` sends Ctrl-U first; `expect_text` waits for a sentinel in settled output) |
| `wait_pane` | wait for a pane to settle or for `expect_text` without sending input |
| `read_pane` | read pane text (`mode`: `"visible"` \| `"scrollback"`); `strip_ansi` defaults to `false`, returning real SGR-styled text for either mode — pass `true` for plain text |
| `read_pane_output` | cursor-based raw PTY transcript (`max_bytes: 0` returns just the current cursor; `since_byte` reads from a saved cursor) |
| `kill_pane` | close a pane; if it is the only pane in a non-final window, close that worker window |
| `set_label` | set the human label (use empty string to clear) |
| `watch_events` | subscribe — server streams session-wide `zmux/event` JSON-RPC notifications for every PaneSpawned/Closed/StateChanged/Output/Exited/LabelChanged. One subscription per connection |

Plus the `zmux://panes` resource for read-only pane snapshots without a tool call.

For agentic turn-taking, prefer a unique sentinel and let `send_keys` do the waiting:

```json
{
  "pane_id": 10001,
  "keys": "run the requested check; echo ZMUX_DONE_042 when finished",
  "enter": true,
  "clear_input": true,
  "expect_text": "ZMUX_DONE_042",
  "max_wait_ms": 60000,
  "wait_lines": 400
}
```

That response includes `text`, `state`, `timed_out`, and `matched_expect`, which is usually enough for a supervising agent to decide whether to continue, retry, or read deeper scrollback. If another controller or hook already sent input, use `wait_pane` with the same `expect_text`/`max_wait_ms`/`wait_lines` shape to observe the turn without injecting more bytes.

### Agent prompt shims

`zmux claude "prompt"` and `zmux codex "prompt"` are convenience wrappers around the MCP tools above. Each auto-starts the selected zmux session, reuses an idle pane with the matching label and startup command, otherwise spawns the real interactive agent CLI in a new window, sends the prompt with a marker contract that avoids echo-matching, waits for the end marker, and prints the marked answer to stdout.

```sh
zmux claude "summarize the failing test"
zmux claude --session claudepass --new --kill "review this diff"
zmux codex --timeout-ms 300000 --label worker-codex "write the docs update"
```

Shared flags: `--session <name>`, `--command <cmd>`, `--label <label>`, `--timeout-ms <ms>`, `--wait-lines <n>`, `--new`, `--kill`, `--keep`, `--worker-json`, `--output-format json`.

Agent startup flags are passed through to the spawned pane, so the wrapper can carry model, permission, sandbox, resume, and directory options accepted by the installed CLI:

```sh
zmux claude --model sonnet --permission-mode bypassPermissions "inspect the current diff"
zmux codex --model gpt-5.4 -C ~/projects/myrepo --dangerously-bypass-approvals-and-sandbox "inspect the current diff"
```

For worker-style callers, `--worker-json`, `--json`, or `--output-format json` prints a single JSON object with `result`, `session_id`, `zmux_session`, `pane_id`, and `killed_after`. A `session_id` like `zmux:default:10000` can be passed back with `--resume` to target that same interactive pane.

```sh
zmux claude --worker-json "summarize the failing test"
zmux codex --worker-json "summarize the failing test"
zmux codex --resume zmux:default:10000 "continue from the previous answer"
```

This is a convenience shim, not a new model backend: it drives the normal interactive agent TUI in a pane. For full reliability, prompts should tolerate the automation marker contract the wrapper appends. The exact end marker is not included in the submitted prompt, so the wrapper waits for the agent's answer rather than the TUI echo of your prompt.

Reusable panes are matched by label, caller cwd, and full agent spawn command. Two calls from `/repo-a` and `/repo-b` with the same label and flags get separate panes. Pass `--new` to force a fresh pane regardless.

#### Resume ids

`session_id` values produced by the shims have the shape `zmux:<session>:<pane_id>`. Pass one back through `--resume` to target the same pane:

```sh
zmux claude --resume zmux:default:10000 "continue from the previous task"
```

For Claude, a plain Claude session UUID is also accepted and passed through to the underlying CLI:

```sh
zmux claude --resume 01234567-89ab-cdef-0123-456789abcdef "continue the Claude conversation"
```

When `--kill` is used, the returned `session_id` is `null` because no pane survives the call.

#### Claude hook capture

`zmux claude` automatically launches Claude with a generated `--settings` file under the zmux state directory; it does not touch your global or project Claude settings. The generated settings install command hooks for `SessionStart`, `UserPromptSubmit`, `Notification`, `Stop`, `StopFailure`, and `SessionEnd`, which append Claude's hook input JSON to:

```text
$ZMUX_STATE_DIR/claude/<zmux-session>/events.jsonl
# or ~/.local/state/zmux/claude/<zmux-session>/events.jsonl
```

Appends are serialized with a zmux-owned lock file so concurrent Claude panes can't interleave records. Each wrapper turn binds a unique nonce: the hook poller waits for a `UserPromptSubmit.prompt` containing that nonce, then accepts a `Stop.last_assistant_message` whose `session_id` and `transcript_path` match. The rendered/raw-transcript marker extraction remains as a fallback for panes that predate hook instrumentation or for non-Claude agents.

If you pass Claude's own `--settings`, zmux reads it, appends its own hooks, writes a merged file, and hands the merged path to Claude — your existing hooks are preserved.

### Manual smoke test

1. `zmux new harness`
2. From a second terminal: `zmux ls --verbose` — should list `harness` and its starter pane with state `Idle`.
3. With Claude Code (or any MCP client) wired up: have it call `spawn_pane` with `command: "claude --print 'hello'"` (or whatever you want to drive). Repeat 3-4 times.
4. Back in the `harness` session, hit `Ctrl-a A` — you should see one row per pane with live status (working → idle as the agents finish).
5. `f` cycles filters; `Enter` attaches to whichever you want to intervene on.
6. `b → y → "follow up: explain that"` → Enter to broadcast a line to every working/idle pane.
7. Tear down with `Ctrl-a A`, `K`, `y` per pane (or `zmux kill harness`).

If any of those steps misbehave (rendering glitches in `claude`/`codex`, missed events, crashed daemon), capture the failing case with `zmux capture <session> <pane> /tmp/repro.bin` and inspect with `cargo run --example replay -- /tmp/repro.bin` — the byte-level diff is usually enough to bisect a VT gap.

## `zmux pair` — local AI co-pilot

`zmux pair --target <pane_id>` launches a hybrid AI co-pilot bound to a sibling pane. The co-pilot watches the target pane's events (errors, non-zero exits, `AwaitingInput`), drops 1–2 sentence proactive notes into its own pane, and accepts free-form chat at a `> ` prompt. It can ask to read scrollback or send keys back to the target pane; every `send_keys` is gated by an explicit user `[y/N]` confirmation.

Backend: [Ollama](https://ollama.com) on `localhost:11434` — no API account required. Default model is `minimax-m2.7:cloud`; override with `--model <name>`.

```
zmux new mywork                            # create + attach
# split a pane (Ctrl-a "), then in the new pane:
zmux pair --session mywork --target 1
```

Pair connects to the same `<session>.mcp.sock` that any external MCP client would, so it doesn't change the daemon.

## Configuration

Optional config at `~/.config/zmux/config.toml`:

```toml
prefix = "ctrl-a"          # rebind the zmux prefix
scrollback = 8192          # scrollback lines per pane
status_hints = true        # show the Ctrl-a hint strip
status_label = "foo"       # override the left-of-status name@host label

[agent]
idle_threshold_ms = 750
shell_prompts = ["$ ", "# ", "> ", "% "]
agent_prompts  = ["│ > ", "architect> ", ">>> "]
```

### Environment variables

| Var | Purpose |
|---|---|
| `ZMUX_STATE_DIR` | override the state directory (default: `$XDG_STATE_HOME/zmux` or `~/.local/state/zmux`). Holds Claude hook event streams, pair-mode per-pane locks, and the per-session MCP audit logs (`audit/<session>.jsonl`). |
| `ZMUX_PAIR_TIMEOUT_SECS` | Ollama HTTP timeout for `zmux pair`, in seconds (default: 60). Bump if your model + scrollback context routinely exceeds it. |
| `ZMUX_PTY_DUMP` | **Debug only** — when set, every PTY ingest appends its raw bytes to the given path. Massive output; use the per-pane `zmux capture` command instead for normal bug repros. |

## Why Rust

- low-level systems control without giving up memory safety
- predictable performance for scrollback-heavy workloads (and for an MCP server that has to coexist with PTY ingestion)
- cleaner modeling of state machines than ad hoc channel-driven code

## Architecture

```
┌──────────────────────────────────────────────────────┐
│  MCP server: tools + watch_events + resources        │  ← external AI clients
├──────────────────────────────────────────────────────┤
│  Agent layer: AgentState, IdleDetector, EventBus     │  ← humans + zmux UI
│  Supervisor overlay (Ctrl-a A)                       │
├──────────────────────────────────────────────────────┤
│  Core: Workspace, Pane, Scrollback, PTY, VT ingester │
└──────────────────────────────────────────────────────┘
```

The daemon stays sync. The MCP listener thread accepts connections, per-conn threads dispatch JSON-RPC over a `mpsc::Sender<McpRequest>` queue, and the render loop drains the queue once per frame on the same thread that owns Workspace. No `Arc<Mutex>` around workspace state; no async leak into Workspace.

## Current limits

- newly-created windows use distinct pane-id ranges so MCP tools can address panes across windows without colliding with the original window.
- the supervisor overlay is session-wide like the MCP surface: it lists every window's panes (foreign rows tagged `w2:`), stays live via the session event bus, and attach/kill/label/broadcast reach any window.
- only one client at a time may be attached; the daemon rejects a second concurrent attach with `Busy`.
- no per-client viewport state yet; the attached client owns the whole workspace.
- the VT subset is targeted at agent CLIs (`claude`, `codex`, `aider`); full xterm/htop/btop parity is not a goal.
- the VT and layout layers are fuzzed for robustness, not correctness: a deterministic fuzz harness (`tests/vt_fuzz.rs`, with `tests/layout_props.rs` and `tests/input_fuzz.rs` covering layout trees and client input) drives hostile byte streams, resizes, and render calls — hundreds of thousands of cases run panic- and hang-free, hostile CSI/OSC payloads are clamped or discarded, and over-constrained layouts clip instead of overlapping. Malformed input degrades the way real terminals degrade (dropped sequences, clipped panes), but unusual-yet-valid sequences may still *render* wrong — if you see misrendering, capture the bytes with `zmux capture <session> <pane> /tmp/repro.bin` and inspect with `cargo run --example replay -- /tmp/repro.bin` so the case can be added to the fixtures. Long soak: `ZMUX_VT_FUZZ_ITERS=200000 cargo test --test vt_fuzz`.
- one MCP `watch_events` subscription per connection (a second call returns a tool-level error).

## Why this exists

zmux is the runtime AI agents live inside. Spawn a swarm of `claude`/`codex`/`aider` instances in panes, watch their state in the supervisor, attach to intervene, broadcast follow-ups to the rest. The MCP server lets external AI clients drive the same surface programmatically — making the muxer itself an agent-controllable workspace, not just a window into one.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
