# zmux

A terminal multiplexer built for running AI coding agents.

zmux does the normal multiplexer things: detached sessions, windows, splits,
scrollback, mouse support, a from-scratch VT engine in Rust with four
dependencies. Then it does the thing tmux can't. Every pane tracks the state
of whatever runs inside it, a supervisor dashboard shows your whole fleet at
a glance, and the daemon itself speaks MCP, so Claude Code or any other MCP
client can spawn panes, type into them, and read them back.

Run six agents in panes. Watch them work. Attach when one needs you. Let
another agent supervise the rest.

## Install

Rust 1.85+:

```sh
cargo build --release
cp target/release/zmux ~/.local/bin/
```

Or grab a prebuilt binary from the
[releases page](https://github.com/zachcampbell/zmux/releases).

## Quick start

```sh
zmux new work        # create a session and attach
zmux new repro --debug-trace  # create with whole-session diagnostics enabled
zmux attach work     # come back later
zmux ls --verbose    # sessions, panes, states, last commands
zmux kill work       # tear it down
```

The prefix is `Ctrl-a`. The ones you'll actually use:

| Key | Action |
|---|---|
| `\|` / `-` | split right / down |
| `o` | next pane |
| `c` / `n` | new window / next window |
| `z` | zoom the active pane |
| `[` | scrollback: vim keys, `/` search, `v` select, `y` yank |
| `A` | supervisor overlay |
| `d` | detach |

The mouse works the way you'd hope: the wheel scrolls the pane under the
cursor one row at a time by default, including primary history behind a
full-screen alternate buffer when the app is not capturing mouse input.
Click-drag selects text in any direction, and releasing the selection copies
it to your clipboard via OSC 52. Hold the drag above or below the pane to
keep selecting through off-screen history. Full binding list, config format,
and CLI reference: [docs/reference.md](docs/reference.md).

## The agent part

Every pane gets an `AgentState` (working, idle, awaiting input, errored,
exited) derived purely from PTY behavior. The tool in the pane doesn't have
to cooperate or even know. `Ctrl-a A` opens the supervisor:

```
┌─ zmux supervisor [working] ─ 3 of 7 panes ─┐
│ ● claude #1     working   "refactor auth"  │
│ ● claude #2     working   tests passed     │
│ ● ollama dev    working   streaming        │
└── j/k navigate · enter attach · b broadcast · k kill ─┘
```

`Enter` attaches to a pane, `K` kills it, `l` labels it, `b` broadcasts a
follow-up prompt to every working pane. Panes in other windows are listed
and reachable too.

### The daemon is an MCP server

Point Claude Code (or any MCP client) at a session:

```json
{
  "mcpServers": {
    "zmux": { "command": "zmux", "args": ["mcp", "--session", "default"] }
  }
}
```

Now the client can call `spawn_pane`, `send_keys`, `read_pane`,
`read_pane_output`, `wait_pane`, `list_panes`, `kill_pane`, `set_label`, and
`watch_events` for live lifecycle notifications. `send_keys` supports
sentinel-based turn taking (`expect_text` plus a timeout), which is usually
all a supervising agent needs to drive an interactive TUI reliably. Details
and examples: [docs/reference.md](docs/reference.md#mcp-server).

### Prompt shims

`zmux claude` and `zmux codex` wrap the MCP tools into one-shot commands that
drive the real interactive CLIs in panes:

```sh
zmux claude "summarize the failing test"
zmux codex --worker-json "write the docs update"
zmux claude --resume zmux:default:10000 "continue from the previous task"
```

They reuse idle panes when they can, support resume ids and JSON output for
worker-style callers, and capture Claude lifecycle hooks for clean result
extraction. See [docs/reference.md](docs/reference.md#agent-prompt-shims).

### Pair mode

`zmux pair --target <pane>` runs a local co-pilot (Ollama, no API account)
in a sibling pane. It watches the target for errors and prompts, comments
proactively, and can send keys back, each send gated behind a `[y/N]`
confirmation.

## Security

zmux is an agent control plane, not a sandbox. Anything that can connect to
the daemon's socket has shell-equivalent authority: spawn commands, type
keys, read every byte of scrollback. There is no per-connection auth beyond
filesystem permissions (`0700` session dir, `0600` sockets). Treat any MCP
client you wire up like a logged-in shell on the same machine. Don't widen
the socket permissions, don't proxy the socket over a network.

Every mutating MCP call is appended to an audit log
(`$ZMUX_STATE_DIR/audit/<session>.jsonl`). That's forensics, not access
control.

## Design

```
┌──────────────────────────────────────────────────────┐
│  MCP server: tools + watch_events + resources        │  <- external AI clients
├──────────────────────────────────────────────────────┤
│  Agent layer: AgentState, IdleDetector, EventBus     │  <- humans + zmux UI
│  Supervisor overlay (Ctrl-a A)                       │
├──────────────────────────────────────────────────────┤
│  Core: Workspace, Pane, Scrollback, PTY, VT ingester │
└──────────────────────────────────────────────────────┘
```

The daemon is synchronous and single-threaded where it matters: MCP
connection threads queue requests over a channel, and the render loop drains
them on the thread that owns the workspace. No `Arc<Mutex>` soup, no async
in the core. The VT engine is written from scratch and covers what agent
CLIs and shells actually emit (alt screen, scroll regions, synchronized
output, bracketed paste, charsets, tab stops, wide chars, combining marks,
joined emoji, the SGR zoo). Full xterm parity is a non-goal.

## Limits

- Multiple clients can attach to one session; they mirror the same view at
  the smallest client's size. Per-client viewports don't exist yet.
- The VT and layout layers are fuzzed for robustness, not rendering
  correctness. If something renders wrong, `zmux trace start <session>`
  captures ordered client input, PTY traffic, resizes, frames, and exact host
  ANSI; `zmux capture` remains available for a small one-pane raw dump. Traces
  are best-effort at the exact stop boundary and contain screen contents and
  secrets, so share them carefully. Detached-daemon diagnostics are retained
  at `$ZMUX_STATE_DIR/logs/<session>.log`.
- One `watch_events` subscription per MCP connection.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
