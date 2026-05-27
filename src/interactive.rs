// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::env;
use std::io;
use std::os::unix::process::ExitStatusExt;

use crate::input::{InputAction, InputParser};
use crate::mouse::{MouseTrackingMode, ScreenMode};
use crate::pty::PtySize;
use crate::session::Session;
use crate::tty::TerminalGuard;

pub fn run_shell() -> io::Result<i32> {
    let mut terminal = TerminalGuard::enter()?;
    let size = terminal.size()?;
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    let mut session =
        Session::spawn_command("shell", &shell, &["-i"], size, 8_192, content_rows(size))?;
    let mut parser = InputParser::default();
    let mut current_size = size;
    let mut dirty = true;

    loop {
        let fresh_size = terminal.size()?;
        if fresh_size != current_size {
            current_size = fresh_size;
            session.resize(current_size, content_rows(current_size))?;
            dirty = true;
        }

        if session.ingest_available_output()? > 0 {
            dirty = true;
        }

        terminal
            .set_mouse_tracking_mode(session.mouse_tracking_mode().max(MouseTrackingMode::Click))?;

        if dirty {
            terminal.render(
                "zmux shell",
                &session.render_lines(),
                &render_status(&session, current_size),
                current_size,
            )?;
            dirty = false;
        }

        if let Some(status) = session.try_wait()? {
            session.ingest_available_output()?;
            terminal.render(
                "zmux shell",
                &session.render_lines(),
                &format!("shell exited with {}", status.code().unwrap_or_default()),
                current_size,
            )?;
            return Ok(status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or_default()));
        }

        let input = terminal.read_input(50)?;
        if input.is_empty() {
            continue;
        }

        for action in parser.push_bytes(&input) {
            match action {
                InputAction::Forward(bytes) => session.write_input(&bytes)?,
                InputAction::Mouse(mouse) => {
                    if mouse.row < content_rows(current_size) as u16
                        && session.handle_mouse_event(mouse)?
                    {
                        dirty = true;
                    }
                    continue;
                }
            }
            dirty = true;
        }
    }
}

fn content_rows(size: PtySize) -> usize {
    size.rows.saturating_sub(1).max(1) as usize
}

fn render_status(session: &Session, size: PtySize) -> String {
    let mode = match session.screen_mode() {
        ScreenMode::Primary if session.follow_output() => "PRIMARY/FOLLOW",
        ScreenMode::Primary => "PRIMARY/SCROLL",
        ScreenMode::Alternate if session.app_captures_mouse() => "ALT/APP-MOUSE",
        ScreenMode::Alternate => "ALT/PANE-WHEEL",
    };

    format!(
        "{mode} | {} lines | {}x{} | wheel scrolls pane | type `exit` to quit",
        session.rendered_line_count(),
        size.cols,
        size.rows
    )
}
