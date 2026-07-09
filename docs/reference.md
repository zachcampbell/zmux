# zmux reference

The long version. For the pitch and quick start, see the [README](../README.md).

## CLI

```
zmux new [name]            create + attach (default name "default")
zmux attach [name]         reattach to an existing session
zmux ls                    list live sessions (hides stale + .mcp.sock entries)
zmux ls --verbose          ls plus per-pane state, last command, last exit
zmux kill [name]           shut a session down (--all kills every live session)
zmux prune [--dry-run]     remove stale session and .mcp.sock files
zmux serve [name]          run the server in the foreground (usually called by new)
zmux capture <s> <p> <f>   dump raw PTY bytes from session s, pane p, into file f
zmux label <s> <p> <text>  set a human label on a pane (use - to clear)
zmux mcp --session <name>  stdio bridge for MCP clients (Claude Code, Cursor, ...)
```

Single-process dev modes: `cargo run --quiet` (bounded PTY demo),
`cargo run -- --shell` (single-pane shell), `cargo run -- --mux`
(two-pane foreground workspace).

Sessions outlive the attached client. The daemon owns PTYs and workspace
state; clients attach over `$TMPDIR/zmux-$USER/<name>.sock`. Starting
`zmux serve` with a name that is already live is refused; it never replaces
the existing session socket.

## Prefix bindings (Ctrl-a, while attached)

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
| `a` | send one literal `Ctrl-a` to the pane |
| `1-9` | jump to window by index |
| `&` | close active window (only when more than one exists) |
| `{` / `}` | swap active pane with the previous / next pane |
| `s` | session-picker overlay (1-9 select, Esc cancels) |
| `,` | rename active pane |
| `!` / `^` | split right / down running a prompted command |
| `z` | zoom (toggle active pane fullscreen) |
| `q` | overlay pane numbers |
| `H` / `J` / `K` / `L` | resize active pane border |
| `2` / `3` / `4` | layout presets (two-col / three-col / four-quad, single-pane only) |
| `Space` | cycle layout presets |
| `y` / `]` | yank visible viewport via OSC 52 / paste last yank |
| `[` | enter scrollback mode |
| `=` | toggle synchronize-panes (mirror keystrokes to every pane in the window) |
| `:` | open the command prompt |
| `A` | open the supervisor overlay |

## Supervisor overlay (Ctrl-a A)

A live dashboard of every pane in the session, including panes in other
windows (tagged `w2:` etc.). Status glyphs: `●` working, `○` idle,
`⚠` awaiting input, `✗` errored, `◐` exited.

| Key | Action |
|---|---|
| `j` / `k` (or arrows) | move selection |
| `Enter` | attach to selected pane (switches windows if needed) |
| `q` / `Esc` | close overlay |
| `f` | cycle filter: all / working / idle / awaiting / errored |
| `K` | kill selected pane (`y` confirms) |
| `l` | edit label of selected pane |
| `b` | broadcast a line to every working/idle pane in the current filter |

## Scrollback mode (Ctrl-a [)

| Key | Action |
|---|---|
| `j` / `k` | line down / up |
| `Ctrl-D` / `Ctrl-U` | half-page down / up |
| `Space` / `b` | full-page down / up |
| `g` / `G` | top / bottom (`G` re-follows live output) |
| `/` | search prompt (Enter commits, Esc cancels) |
| `n` / `N` | next / previous match |
| `v` / `V` / `R` | begin selection: character / line / rectangle |

In selection mode, movement keys extend the selection; `y` yanks via OSC 52
and exits; `Esc` cancels. Scrollback remains available while an alternate-
screen program is active: scrolling up reveals the retained primary history,
and `G` restores the live alternate buffer. If the application enabled mouse
tracking, wheel events inside its pane are forwarded to the application.
On the primary screen, a mostly vertical unmodified drag scrolls the viewport.
This also supports touchscreen swipes when the host terminal reports them as
mouse drags. Horizontal drags select text; hold Shift to force selection for a
vertical drag.

## Command prompt (Ctrl-a :)

| Command | Effect |
|---|---|
| `display-message <text>` | flash the text in the status bar |
| `capture <session> <pane> <path>` | dump raw PTY bytes for a pane to a file |

The prompted-split bindings (`Ctrl-a !` and `Ctrl-a ^`) accept any shell
command line; `Ctrl-a :` is only for the registered commands above.

## Configuration

Optional config at `~/.config/zmux/config.toml`:

```toml
prefix = "ctrl-a"          # rebind the zmux prefix
scrollback = 8192          # scrollback lines per pane
wheel_scroll_lines = 1     # rows per wheel event (positive integer)
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
| `ZMUX_STATE_DIR` | override the state directory (default `$XDG_STATE_HOME/zmux` or `~/.local/state/zmux`). Holds daemon logs, Claude hook event streams, pair-mode locks, and MCP audit logs. |
| `ZMUX_PAIR_TIMEOUT_SECS` | Ollama HTTP timeout for `zmux pair` in seconds (default 60). |
| `ZMUX_PTY_DUMP` | debug only: append every PTY ingest's raw bytes to the given path. Prefer `zmux capture` for bug repros. |

Detached sessions append daemon diagnostics to
`$ZMUX_STATE_DIR/logs/<session>.log`. State directories are created mode
`0700`; daemon logs, hook/settings files, and MCP audit logs are mode `0600`.
Foreground `zmux serve` continues to write diagnostics to its inherited
stderr.

## MCP server

The daemon exposes a JSON-RPC 2.0 MCP server (protocol version `2025-06-18`)
on `$TMPDIR/zmux-$USER/<session>.mcp.sock`. Use `zmux mcp --session <name>`
as a stdio bridge for clients that only speak stdio. Read the
[security section](../README.md#security) before wiring up a client.

| Tool | Purpose |
|---|---|
| `list_panes` | every pane with window index, state, label, last command, last exit, size |
| `spawn_pane` | spawn a pane running a command (`split`: `"h"`, `"v"`, `"window"`; window spawns land in the background) |
| `send_keys` | type into a pane (`enter: true` presses Enter; `clear_input` sends Ctrl-U first; `expect_text` waits for a sentinel) |
| `wait_pane` | wait for a pane to settle or for `expect_text`, without sending input |
| `read_pane` | pane text (`mode`: `"visible"` or `"scrollback"`); `strip_ansi` defaults to false (real SGR-styled text) |
| `read_pane_output` | cursor-based raw PTY transcript (`max_bytes: 0` returns just the cursor; `since_byte` resumes) |
| `kill_pane` | close a pane; a sole pane in a non-final window closes that window |
| `set_label` | set the human label (empty string clears) |
| `watch_events` | subscribe to session-wide `zmux/event` notifications (PaneSpawned/Closed/StateChanged/Output/Exited/LabelChanged); one subscription per connection |

There is also a `zmux://panes` resource for read-only snapshots without a
tool call.

For agentic turn-taking, prefer a unique sentinel and let `send_keys` do the
waiting:

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

The response includes `text`, `state`, `timed_out`, and `matched_expect`,
which is usually enough for a supervising agent to decide whether to
continue, retry, or read deeper scrollback. If another controller already
sent input, use `wait_pane` with the same shape to observe without injecting
more bytes.

### Audit log

Every mutating MCP call (`send_keys`, `spawn_pane`, `kill_pane`,
`set_label`) is appended as a JSON line to
`$ZMUX_STATE_DIR/audit/<session>.jsonl` (mode `0600`) with a timestamp and a
per-connection id, so concurrent controllers can be told apart after the
fact. Keystroke payloads are truncated at 2 KiB. This records what happened;
it does not prevent anything.

## Agent prompt shims

`zmux claude "prompt"` and `zmux codex "prompt"` wrap the MCP tools into
one-shot commands. Each auto-starts the session, reuses an idle pane whose
label, caller cwd, and spawn command match, otherwise spawns the agent CLI
in a new window, sends the prompt with a marker contract that avoids
echo-matching, waits for the end marker, and prints the answer to stdout.

```sh
zmux claude "summarize the failing test"
zmux claude --session claudepass --new --kill "review this diff"
zmux codex --timeout-ms 300000 --label worker-codex "write the docs update"
```

Shared flags: `--session <name>`, `--command <cmd>`, `--label <label>`,
`--timeout-ms <ms>`, `--wait-lines <n>`, `--new`, `--kill`, `--keep`,
`--worker-json`, `--output-format json`.

Agent startup flags pass through to the spawned pane, so model, permission,
sandbox, resume, and directory options work as the installed CLI defines
them:

```sh
zmux claude --model sonnet --permission-mode bypassPermissions "inspect the current diff"
zmux codex --model gpt-5.4 -C ~/projects/myrepo "inspect the current diff"
```

For worker-style callers, `--worker-json` (or `--output-format json`) prints
a single JSON object with `result`, `session_id`, `zmux_session`, `pane_id`,
and `killed_after`.

### Resume ids

`session_id` values have the shape `zmux:<session>:<pane_id>`. Pass one back
through `--resume` to target the same pane:

```sh
zmux claude --resume zmux:default:10000 "continue from the previous task"
```

For Claude, a plain Claude session UUID is also accepted and passed through
to the underlying CLI. With `--kill`, the returned `session_id` is `null`
because no pane survives the call.

### Claude hook capture

`zmux claude` launches Claude with a generated `--settings` file under the
zmux state directory; it does not touch your global or project settings. The
generated settings install hooks (`SessionStart`, `UserPromptSubmit`,
`Notification`, `Stop`, `StopFailure`, `SessionEnd`) that append hook JSON
to `$ZMUX_STATE_DIR/claude/<zmux-session>/events.jsonl`, serialized with a
lock file so concurrent panes can't interleave records.

Each wrapper turn binds a unique nonce: the poller waits for a
`UserPromptSubmit.prompt` containing the nonce, then accepts a
`Stop.last_assistant_message` whose `session_id` and `transcript_path`
match. Marker extraction from the rendered transcript remains as a fallback
for panes without hook instrumentation and for non-Claude agents.

If you pass your own `--settings`, zmux merges its hooks into a copy and
hands Claude the merged file; your hooks are preserved.

## Pair mode

`zmux pair --target <pane_id>` launches a co-pilot bound to a sibling pane.
It watches the target's events (errors, non-zero exits, awaiting-input),
drops short proactive notes into its own pane, and accepts free-form chat at
a `> ` prompt. It can read scrollback and send keys to the target; every
`send_keys` requires an explicit `[y/N]` confirmation.

Backend: [Ollama](https://ollama.com) on `localhost:11434`. Default model
`minimax-m2.7:cloud`; override with `--model <name>`.

```sh
zmux new mywork
# split a pane, then in the new pane:
zmux pair --session mywork --target 1
```

Pair connects to the same `<session>.mcp.sock` as any external MCP client;
the confirmation gate lives in pair's UX, not in the MCP layer.

## Debugging rendering issues

Capture the failing pane's raw bytes and replay them:

```sh
zmux capture <session> <pane> /tmp/repro.bin
cargo run --example replay -- /tmp/repro.bin
```

The pane id is validated before the capture path is created, so a typo does
not create or truncate the requested file. For daemon-side errors from a
detached session, inspect `$ZMUX_STATE_DIR/logs/<session>.log`.

The VT and layout layers are fuzzed (`tests/vt_fuzz.rs`,
`tests/layout_props.rs`, `tests/input_fuzz.rs`): hostile byte streams,
resizes, and render calls run panic- and hang-free, and malformed input
degrades the way real terminals degrade. Unusual-but-valid sequences may
still render wrong; captured repros become fixtures. Long soak:
`ZMUX_VT_FUZZ_ITERS=200000 cargo test --test vt_fuzz`.

## Manual smoke test

1. `zmux new harness`
2. From a second terminal: `zmux ls --verbose` shows `harness` with an Idle pane.
3. With an MCP client wired up, call `spawn_pane` a few times with real commands.
4. `Ctrl-a A` in the session: one row per pane, live status transitions.
5. `f` cycles filters; `Enter` attaches; `b` broadcasts a follow-up.
6. Tear down with `zmux kill harness`.
