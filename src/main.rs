// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::env;
use std::io;
use std::process::ExitCode;

use zmux::mcp::run_stdio_bridge;
use zmux::{
    AttachOutcome, ClientMessage, PtySize, Session, attach_session, create_session,
    default_session_name, kill_session, list_sessions, list_sessions_verbose, print_session_list,
    print_session_list_verbose, prune_stale_sessions, run_mux, run_server, run_shell,
    send_admin_message,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Display, not Debug: the default `Result` formatter
            // surfaces raw OS error codes which aren't actionable.
            eprintln!("zmux: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") => {
            let name = session_name(&args);
            let exit_code = run_server(name).map_err(|err| format!("server `{name}`: {err}"))?;
            println!("zmux server `{name}` exited with status {exit_code}");
            Ok(())
        }
        Some("new") => {
            let name = session_name(&args);
            create_session(name).map_err(|err| friendly_create_error(name, &err))?;
            attach(name)
        }
        Some("attach") => attach(session_name(&args)),
        Some("ls") => {
            if args.iter().any(|a| a == "--verbose" || a == "-v") {
                let detail =
                    list_sessions_verbose().map_err(|err| format!("listing sessions: {err}"))?;
                print_session_list_verbose(&detail, &mut io::stdout())
                    .map_err(|err| format!("printing session list: {err}"))
            } else {
                let sessions = list_sessions().map_err(|err| format!("listing sessions: {err}"))?;
                print_session_list(&sessions, &mut io::stdout())
                    .map_err(|err| format!("printing session list: {err}"))
            }
        }
        Some("prune") => {
            let mut dry_run = false;
            for arg in args.iter().skip(2) {
                match arg.as_str() {
                    "--dry-run" | "-n" => dry_run = true,
                    "--help" | "-h" => {
                        print_prune_usage();
                        return Ok(());
                    }
                    other => return Err(format!("prune: unknown argument `{other}`")),
                }
            }

            let report =
                prune_stale_sessions(dry_run).map_err(|err| format!("pruning sessions: {err}"))?;
            if report.removed.is_empty() {
                println!("no stale sessions");
            } else {
                if dry_run {
                    println!("would remove stale socket files:");
                } else {
                    println!("removed stale socket files:");
                }
                for path in report.removed {
                    println!("  {}", path.display());
                }
            }
            Ok(())
        }
        Some("kill") => {
            let kill_all = args.iter().skip(2).any(|a| a == "--all");
            let named = args.iter().skip(2).any(|a| !a.starts_with("--"));
            if kill_all && named {
                return Err("kill: --all does not take a session name".to_string());
            }
            if kill_all {
                let sessions = list_sessions().map_err(|err| format!("listing sessions: {err}"))?;
                if sessions.is_empty() {
                    println!("no live sessions");
                    return Ok(());
                }
                let mut failures: Vec<String> = Vec::new();
                for entry in &sessions {
                    match kill_session(&entry.name) {
                        Ok(()) => println!("sent shutdown to session `{}`", entry.name),
                        // Race between list and kill: session went away on
                        // its own. Treat as a successful no-op.
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {
                            println!("session `{}` already gone", entry.name);
                        }
                        Err(err) => {
                            failures.push(format!("{}: {err}", entry.name));
                        }
                    }
                }
                if !failures.is_empty() {
                    return Err(format!(
                        "failed to kill {} session(s): {}",
                        failures.len(),
                        failures.join("; ")
                    ));
                }
                Ok(())
            } else {
                let name = session_name(&args);
                kill_session(name).map_err(|err| friendly_kill_error(name, &err))?;
                println!("sent shutdown to session `{name}`");
                Ok(())
            }
        }
        Some("label") => {
            let session = session_name(&args);
            let pane_id: u32 = args
                .get(3)
                .ok_or_else(|| {
                    "label: missing pane id (zmux label <session> <pane> <label>)".to_string()
                })?
                .parse()
                .map_err(|_| "label: pane id must be an integer".to_string())?;
            // An empty label or a literal `-` clears the label.
            let raw = args
                .get(4)
                .ok_or_else(|| "label: missing label string".to_string())?;
            let label = if raw == "-" || raw.is_empty() {
                None
            } else {
                Some(raw.clone())
            };
            send_admin_message(session, ClientMessage::SetLabel { pane_id, label })
                .map_err(|err| format!("label on `{session}`: {err}"))?;
            // Fire-and-forget admin protocol: a successful socket
            // write only confirms the daemon received the request.
            println!(
                "label request sent to session `{session}` pane {pane_id}; \
                 check the daemon's stderr or `zmux ls --verbose` to confirm",
            );
            Ok(())
        }
        Some("capture") => {
            let session = session_name(&args);
            let pane_id: u32 = args
                .get(3)
                .ok_or_else(|| {
                    "capture: missing pane id (zmux capture <session> <pane> <path>)".to_string()
                })?
                .parse()
                .map_err(|_| "capture: pane id must be an integer".to_string())?;
            let path = args
                .get(4)
                .ok_or_else(|| "capture: missing output path".to_string())?
                .to_string();
            send_admin_message(session, ClientMessage::Capture { pane_id, path })
                .map_err(|err| format!("capture on `{session}`: {err}"))?;
            // Fire-and-forget: a successful socket write only
            // confirms delivery, not that the pane existed or that
            // the file was creatable. Failures land on the daemon's
            // stderr.
            println!(
                "capture request sent to session `{session}` pane {pane_id}; \
                 check the daemon's stderr to confirm the sink attached"
            );
            Ok(())
        }
        Some("claude-hook") => zmux::claude_hooks::run_cli(&args),
        Some("claude") => match zmux::claude::parse_args(&args, default_session_name()) {
            Ok(parsed) => zmux::claude::run(parsed),
            Err(message) if message.starts_with("claude usage:") => {
                println!("{message}");
                Ok(())
            }
            Err(message) => Err(message),
        },
        Some("codex") => match zmux::codex::parse_args(&args, default_session_name()) {
            Ok(parsed) => zmux::codex::run(parsed),
            Err(message) if message.starts_with("codex usage:") => {
                println!("{message}");
                Ok(())
            }
            Err(message) => Err(message),
        },
        Some("mcp") => {
            // Stdio adapter: client speaks JSON-RPC over our
            // stdin/stdout; we bridge to the session's `*.mcp.sock`.
            let session = args
                .iter()
                .position(|a| a == "--session")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str)
                .unwrap_or(default_session_name());
            run_stdio_bridge(session).map_err(|err| format!("mcp bridge for `{session}`: {err}"))
        }
        Some("pair") => {
            let parsed = zmux::pair::parse_args(&args, default_session_name())?;
            zmux::pair::run(parsed)
        }
        Some("--mux") => {
            let exit_code = run_mux().map_err(|err| format!("running mux: {err}"))?;
            println!("zmux mux exited with status {exit_code}");
            Ok(())
        }
        Some("--shell") => {
            let exit_code = run_shell().map_err(|err| format!("running shell: {err}"))?;
            println!("zmux shell exited with status {exit_code}");
            Ok(())
        }
        Some("--help" | "-h" | "help") => {
            print_usage();
            Ok(())
        }
        _ => demo().map_err(|err| format!("running demo: {err}")),
    }
}

fn attach(name: &str) -> Result<(), String> {
    // Loop on `AttachOutcome::Switch` so a chain of session
    // switches (A → B → C → detach) feels continuous to the user.
    let mut current = name.to_string();
    loop {
        match attach_session(&current).map_err(|err| friendly_attach_error(&current, &err))? {
            AttachOutcome::Detached => {
                println!("detached from session `{current}`");
                return Ok(());
            }
            AttachOutcome::Exited(code) => {
                println!("session `{current}` exited with status {code}");
                return Ok(());
            }
            AttachOutcome::Switch(target) => {
                current = target;
            }
        }
    }
}

fn session_name(args: &[String]) -> &str {
    args.get(2)
        .map(String::as_str)
        .unwrap_or(default_session_name())
}

fn friendly_create_error(name: &str, err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::AlreadyExists => format!("session `{name}` already exists"),
        io::ErrorKind::TimedOut => {
            format!("session `{name}` failed to start within the startup timeout")
        }
        io::ErrorKind::InvalidInput => format!("session name `{name}` is not valid"),
        _ => format!("creating session `{name}`: {err}"),
    }
}

fn friendly_kill_error(name: &str, err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::NotFound => format!("no session named `{name}`"),
        io::ErrorKind::InvalidInput => format!("session name `{name}` is not valid"),
        _ => format!("killing session `{name}`: {err}"),
    }
}

fn friendly_attach_error(name: &str, err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::NotFound => format!("no session named `{name}`"),
        io::ErrorKind::ConnectionRefused => {
            format!("session `{name}` has a socket but no live server (try `zmux kill {name}`)",)
        }
        io::ErrorKind::AddrInUse => {
            format!("session `{name}` is already attached by another client")
        }
        io::ErrorKind::InvalidInput => format!("session name `{name}` is not valid"),
        _ => format!("attaching to session `{name}`: {err}"),
    }
}

fn print_prune_usage() {
    println!("Usage: zmux prune [-n|--dry-run]");
    println!("Remove stale zmux session and MCP socket files from the session directory.");
    println!("Use --dry-run to preview the files without deleting them.");
}

fn print_usage() {
    println!("zmux — terminal multiplexer with pane-local mouse wheel scrolling\n");
    println!("Usage:");
    println!("  zmux new [name]       create and attach to a detached session");
    println!("  zmux attach [name]    attach to an existing detached session");
    println!("  zmux ls [--verbose]   list live sessions (--verbose includes per-pane state)");
    println!("  zmux prune [-n|--dry-run]  remove stale session socket files");
    println!("  zmux kill [name]      end a detached session");
    println!("  zmux kill --all       end every live session");
    println!("  zmux claude [flags] \"prompt\"  drive/reuse an interactive Claude pane");
    println!("  zmux codex  [flags] \"prompt\"  drive/reuse an interactive Codex pane");
    println!("  zmux capture <session> <pane> <path>   dump raw PTY bytes from a pane to a file");
    println!(
        "  zmux label   <session> <pane> <label>  set a pane's label (use - or empty to clear)"
    );
    println!("  zmux serve [name]     run a server in the foreground (usually invoked by new)");
    println!(
        "  zmux mcp --session <name>   stdio bridge for MCP clients (Claude Code, Cursor, ...)"
    );
    println!(
        "  agent shim flags: --session <name> --new --kill --timeout-ms <ms> --command <cmd> --worker-json"
    );
    println!("  zmux --shell          single-pane interactive shell wrapper");
    println!("  zmux --mux            two-pane foreground workspace");
    println!();
    println!("While attached, the Ctrl-a prefix binds:");
    println!("  d  detach      |  split right      -  split down");
    println!("  x  close pane  o  next pane        p  previous pane");
    println!("  c  new window  n  next window      P  previous window");
    println!("  &  close active window (only when multiple windows exist)");
    println!("  {{  swap with previous pane         }}    swap with next pane");
    println!("  s  pick another session from an overlay (1-9 select, Esc cancels)");
    println!("  ,  rename active pane (Enter commits, Esc cancels)");
    println!("  !  split right running a prompted command (Enter runs, Esc cancels)");
    println!("  ^  split down  running a prompted command (Enter runs, Esc cancels)");
    println!("  z  zoom (toggle active pane fullscreen)");
    println!("  A  open supervisor overlay (j/k navigate, Enter attach,");
    println!("     l label, K kill, q close)");
    println!("  q  pane numbers overlay            H/L  resize horizontal");
    println!("                                     K/J  resize vertical");
    println!("  1-9  select window by index       Space cycles layout presets");
    println!("       (two-columns → three-columns → four-quadrants)");
    println!("  ]  paste the last yanked text into the active pane");
    println!();
    println!("In scrollback mode (Ctrl-a [):");
    println!("  j/k page line by line, Ctrl-D/Ctrl-U half-page, g/G top/bottom");
    println!("  /  open search prompt; Enter commits, Esc cancels");
    println!("  n  jump to next match,  N  jump to previous match");
    println!("  v  begin line selection; motion keys extend the selected range");
    println!("     y copies selection to clipboard and exits, Esc cancels");
    println!();
    println!("Config file: ~/.config/zmux/config.toml (optional)");
    println!("  prefix = \"ctrl-a\"       # rebind the zmux prefix");
    println!("  scrollback = 8192        # scrollback lines per pane");
    println!("  status_hints = true      # show the Ctrl-a hint strip");
    println!("  status_label = \"foo\"    # override the left-of-status name@host label");
}

fn print_snapshot(label: &str, lines: &[String]) {
    println!();
    println!("{label}:");
    for line in lines {
        println!("  {line}");
    }
}

fn demo() -> Result<(), Box<dyn std::error::Error>> {
    let session = Session::spawn_command(
        "demo-command",
        "/bin/sh",
        &[
            "-lc",
            "printf 'booting\\n'; printf '\\033[34mready\\033[0m\\n'; printf 'tail line without newline';",
        ],
        PtySize::new(24, 80),
        64,
        6,
    )?;
    let completed = session.drain_to_completion()?;

    println!("zmux PTY demo");
    println!(
        "command exited successfully: {}",
        completed.exit_status().success()
    );
    print_snapshot("pane lines", &completed.pane().visible_text());
    println!();
    println!("run `zmux --help` for the full command list");
    Ok(())
}
