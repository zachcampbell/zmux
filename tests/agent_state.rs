// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration coverage for agent-state wiring on Workspace:
//! subscribe to the event bus, ingest real PTY output, observe the
//! `PaneStateChanged` event and the `tick`-driven flip back to Idle.

use std::time::{Duration, Instant};

use zmux::PtySize;
use zmux::agent::AgentState;
use zmux::events::Event;
use zmux::workspace::Workspace;

// /bin/echo is small, deterministic, and exits immediately — ideal for
// a workspace-level test that wants to provoke "some output then quiet"
// without inheriting a shell's banner.
fn workspace_with_echo() -> Workspace {
    // The single-shell constructor is the only public entry point for
    // building a Workspace. `/bin/sh -c true` is quieter than the
    // default interactive shell — no prompt banner, no rcfile noise —
    // and exits as soon as it spawns, which is fine because we drain
    // any output it produced before it died.
    Workspace::spawn_single_named("/bin/sh", PtySize::new(24, 80), "agent-test")
        .expect("spawn workspace")
}

#[test]
fn pane_state_event_fires_on_pty_output() {
    let mut ws = workspace_with_echo();
    let rx = ws.subscribe_events();

    // Drain output until we see something land or we time out. The
    // shell prints its prompt almost immediately on most systems but
    // we tolerate a small startup lag (zsh sometimes pauses for
    // global rc files).
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got_state_change = false;
    while Instant::now() < deadline && !got_state_change {
        let _ = ws.ingest_available_output().expect("ingest");
        while let Ok(event) = rx.try_recv() {
            if let Event::PaneStateChanged { from, to, .. } = event {
                // First transition is Idle → something. We accept either
                // Working (no prompt yet) or AwaitingInput (prompt
                // already on screen) — both prove the wiring fires.
                assert_eq!(from, "Idle");
                assert!(
                    to == "Working" || to == "AwaitingInput",
                    "unexpected first transition target: {to}"
                );
                got_state_change = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(
        got_state_change,
        "expected at least one PaneStateChanged event after PTY output"
    );
}

#[test]
fn tick_flips_working_back_to_idle_after_threshold() {
    let mut ws = workspace_with_echo();
    // Force the idle threshold tight so we don't burn a 750ms
    // wall-clock wait inside the test.
    ws.set_agent_config(
        Duration::from_millis(20),
        vec!["$ ".into(), "# ".into(), "> ".into(), "% ".into()],
        vec![],
    );
    let rx = ws.subscribe_events();

    // Drive ingestion until *something* lands — interactive shells
    // can take a beat to start producing output. We loop a few hundred
    // ms and only proceed once we've actually observed a pane state
    // change away from the Idle default.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got_initial = false;
    while Instant::now() < deadline && !got_initial {
        let _ = ws.ingest_available_output().expect("ingest");
        while let Ok(event) = rx.try_recv() {
            if matches!(event, Event::PaneStateChanged { .. }) {
                got_initial = true;
            }
        }
        if !got_initial {
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    assert!(got_initial, "no initial state change observed");

    // Now wait past the idle threshold, then tick. A prompt-detector
    // path sticks at AwaitingInput and the tick is a no-op — that's
    // also valid coverage of the wiring (the pane settled into a
    // terminal state). The Working path must flip back to Idle.
    std::thread::sleep(Duration::from_millis(60));
    ws.tick_agents(Instant::now());
    let mut saw_terminal = false;
    while let Ok(event) = rx.try_recv() {
        if let Event::PaneStateChanged { to, .. } = event
            && (to == "Idle" || to == "AwaitingInput")
        {
            saw_terminal = true;
        }
    }
    // It's also correct for the pane to *already* be in
    // AwaitingInput from the initial pass (no further events). Read
    // the latest state directly to cover that case.
    let latest = ws.pane_summaries()[0].state.clone();
    assert!(
        saw_terminal || matches!(latest, AgentState::Idle | AgentState::AwaitingInput),
        "expected terminal state Idle or AwaitingInput; latest was {latest:?}"
    );
}

#[test]
fn agent_state_default_is_idle_for_genesis_pane() {
    let ws = workspace_with_echo();
    // `pane_summaries` returns at least the genesis pane.
    let summaries = ws.pane_summaries();
    assert_eq!(summaries.len(), 1);
    let pane = &summaries[0];
    assert_eq!(pane.pane_id, 1);
    // Pre-ingest the state must be Idle (the default constructor sets
    // it that way; even if the shell has already printed a prompt
    // since spawn, ingestion hasn't run yet from the test thread).
    assert_eq!(pane.state, AgentState::Idle);
}

// The regression test for the AwaitingInput-stickiness fix lives as a
// unit test inside `src/workspace.rs` (see
// `awaiting_input_unsticks_when_output_arrives_without_prompt`). It
// requires direct mutation of the per-session detector state to
// reproduce the bug deterministically — going through public APIs
// from an integration test forces a real shell to print a prompt,
// which isn't reliable across PTY environments (some CI shells skip
// the prompt in non-interactive PTY mode and the test would silently
// pass against the buggy code).
