// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent layer: pane state model and detectors.
//!
//! `AgentState` is derived from observed PTY behavior. No agent CLI
//! cooperation is required — the workspace polls each pane's
//! `IdleDetector` on every render frame and consults `PromptDetector`
//! against the rendered last visible line. The detectors are kept
//! tiny and synchronous so the existing render loop pays no allocation
//! or scheduling cost when nothing has changed.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    AwaitingInput,
    Errored,
    Exited(i32),
}

impl AgentState {
    /// Stable wire string used by `Event::PaneStateChanged` and
    /// `PaneSummary::state`. Mirrors the `Debug` form for the simple
    /// variants and renders `Exited(N)` as `Exited(N)` so MCP consumers
    /// can pattern-match on it.
    pub fn as_wire(&self) -> String {
        match self {
            AgentState::Idle => "Idle".to_string(),
            AgentState::Working => "Working".to_string(),
            AgentState::AwaitingInput => "AwaitingInput".to_string(),
            AgentState::Errored => "Errored".to_string(),
            AgentState::Exited(code) => format!("Exited({code})"),
        }
    }
}

/// Tracks the wall-clock interval since the most recent PTY output and
/// flips the pane back to `Idle` once it has been quiet for at least
/// `threshold`. The detector itself only knows about Idle/Working;
/// AwaitingInput and Errored are pushed in by the workspace via
/// `force_state` so the prompt detector can win.
#[derive(Debug)]
pub struct IdleDetector {
    threshold: Duration,
    last_output: Instant,
    state: AgentState,
}

impl IdleDetector {
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            last_output: Instant::now(),
            state: AgentState::Idle,
        }
    }

    /// Record that a chunk of PTY output just landed. Bumps the
    /// detector to `Working` if it was previously `Idle`. Other states
    /// (AwaitingInput, Errored, Exited) are honored — the workspace
    /// will reset them via `force_state` if the new output overrides
    /// the prompt/error indication.
    pub fn note_output(&mut self, now: Instant) {
        self.last_output = now;
        if self.state == AgentState::Idle {
            self.state = AgentState::Working;
        }
    }

    /// Called from the workspace tick. Returns `Some(Idle)` exactly
    /// once on the transition Working → Idle so the workspace knows
    /// to publish `PaneStateChanged`.
    pub fn tick(&mut self, now: Instant) -> Option<AgentState> {
        if self.state == AgentState::Working
            && now.saturating_duration_since(self.last_output) >= self.threshold
        {
            self.state = AgentState::Idle;
            return Some(AgentState::Idle);
        }
        None
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Override the detector's state. Used by the workspace when
    /// the prompt detector trips (push to `AwaitingInput`) or when a
    /// child exits (push to `Exited`). The detector keeps tracking
    /// outputs after a force; the next `note_output` will lift it
    /// back into `Working` if appropriate.
    pub fn force_state(&mut self, state: AgentState) {
        self.state = state;
    }
}

/// Recognises a pane's last visible line as a prompt-style marker
/// (shell prompt, agent CLI prompt). Patterns are a flat list checked
/// in order; first match wins. Suffix-match because most prompts end
/// with a sentinel character followed by a space (`$ `, `# `,
/// `architect> `).
#[derive(Debug)]
pub struct PromptDetector {
    shell_patterns: Vec<String>,
    agent_patterns: Vec<String>,
}

impl PromptDetector {
    pub fn new(shell_patterns: Vec<String>, agent_patterns: Vec<String>) -> Self {
        Self {
            shell_patterns,
            agent_patterns,
        }
    }

    pub fn defaults() -> Self {
        Self {
            shell_patterns: vec!["$ ".into(), "# ".into(), "> ".into(), "% ".into()],
            agent_patterns: vec![
                "│ > ".into(),        // claude code prompt frame
                "architect> ".into(), // aider
                ">>> ".into(),        // ipython-style agents
            ],
        }
    }

    /// Returns true if the trimmed last line ends with a known prompt.
    pub fn is_prompt(&self, last_line: &str) -> bool {
        let trimmed = last_line.trim_end();
        if trimmed.is_empty() {
            return false;
        }
        for p in self.shell_patterns.iter().chain(self.agent_patterns.iter()) {
            // Compare against both the as-typed pattern and a
            // right-trimmed variant — config-loaded patterns may
            // legitimately end with a space, but the same pattern
            // mid-line might be trimmed by the renderer.
            if trimmed.ends_with(p.trim_end()) || trimmed.ends_with(p.as_str()) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_to_working_on_first_output() {
        let start = Instant::now();
        let mut det = IdleDetector::new(Duration::from_millis(750));
        assert_eq!(det.state(), &AgentState::Idle);
        det.note_output(start);
        assert_eq!(det.state(), &AgentState::Working);
    }

    #[test]
    fn working_to_idle_after_threshold() {
        let start = Instant::now();
        let mut det = IdleDetector::new(Duration::from_millis(100));
        det.note_output(start);
        assert_eq!(det.tick(start + Duration::from_millis(50)), None);
        assert_eq!(
            det.tick(start + Duration::from_millis(150)),
            Some(AgentState::Idle)
        );
    }

    #[test]
    fn force_state_overrides_detector() {
        let mut det = IdleDetector::new(Duration::from_millis(750));
        det.force_state(AgentState::Errored);
        assert_eq!(det.state(), &AgentState::Errored);
    }

    #[test]
    fn agent_state_wire_format() {
        assert_eq!(AgentState::Idle.as_wire(), "Idle");
        assert_eq!(AgentState::Working.as_wire(), "Working");
        assert_eq!(AgentState::AwaitingInput.as_wire(), "AwaitingInput");
        assert_eq!(AgentState::Errored.as_wire(), "Errored");
        assert_eq!(AgentState::Exited(7).as_wire(), "Exited(7)");
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn detects_shell_prompt() {
        let det = PromptDetector::defaults();
        assert!(det.is_prompt("zach@host:~/code $ "));
        assert!(det.is_prompt("# "));
    }

    #[test]
    fn detects_aider_prompt() {
        let det = PromptDetector::defaults();
        assert!(det.is_prompt("architect> "));
    }

    #[test]
    fn does_not_match_random_line() {
        let det = PromptDetector::defaults();
        assert!(!det.is_prompt("running tests..."));
        assert!(!det.is_prompt(""));
    }
}
