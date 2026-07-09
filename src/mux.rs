// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::env;
use std::io;

use crate::input::InputParser;
use crate::tty::TerminalGuard;
use crate::workspace::Workspace;

pub fn run_mux() -> io::Result<i32> {
    let config = crate::config::Config::load();
    let mut terminal = TerminalGuard::enter()?;
    let size = terminal.size()?;
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    let mut workspace = Workspace::spawn_two_pane(&shell, size)?;
    workspace.set_wheel_scroll_lines(config.wheel_scroll_lines);
    let mut parser = InputParser::default();
    let mut current_size = size;
    let mut dirty = true;

    loop {
        let fresh_size = terminal.size()?;
        if fresh_size != current_size {
            current_size = fresh_size;
            workspace.resize(current_size)?;
            dirty = true;
        }

        if workspace.ingest_available_output()? {
            dirty = true;
        }

        if workspace.update_exit_statuses()? {
            dirty = true;
        }

        terminal.set_mouse_tracking_mode(workspace.mouse_tracking_mode())?;

        if dirty {
            terminal.render_frame(
                &workspace.render_frame(),
                current_size,
                workspace.cursor_screen_position(),
            )?;
            dirty = false;
        }

        if let Some(exit_code) = workspace.exit_code_if_complete() {
            return Ok(exit_code);
        }

        let input = terminal.read_input(50)?;
        if input.is_empty() {
            continue;
        }

        for action in parser.push_bytes(&input) {
            if workspace.handle_input(action)? {
                dirty = true;
            }
        }
    }
}
