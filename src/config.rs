// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// Tiny custom config-file format (no `toml` / `serde` dep): one
// `key = value` per line, `#` starts a comment, strings are quoted.
// Unknown keys and section names are silently ignored so older
// configs keep loading. An optional `[agent]` section dispatches its
// keys to `apply_agent_entry`.

pub const DEFAULT_PREFIX_BYTE: u8 = 0x01; // Ctrl-a
pub const DEFAULT_SCROLLBACK_LINES: usize = 8_192;
pub const DEFAULT_STATUS_BAR_HINTS: bool = true;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub idle_threshold_ms: u64,
    pub shell_prompts: Vec<String>,
    pub agent_prompts: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            idle_threshold_ms: 750,
            shell_prompts: vec!["$ ".into(), "# ".into(), "> ".into(), "% ".into()],
            agent_prompts: vec!["│ > ".into(), "architect> ".into(), ">>> ".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub prefix_byte: u8,
    pub scrollback_lines: usize,
    pub status_bar_hints: bool,
    // Overrides the left-of-status label. `None` means fall back to the
    // default `{session_name}@{hostname}`. Loaded from `status_label`
    // in the config file.
    pub status_label: Option<String>,
    pub agent: AgentConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix_byte: DEFAULT_PREFIX_BYTE,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            status_bar_hints: DEFAULT_STATUS_BAR_HINTS,
            status_label: None,
            agent: AgentConfig::default(),
        }
    }
}

impl Config {
    // Load from `$XDG_CONFIG_HOME/zmux/config.toml` (or `$HOME/.config/...`),
    // falling back silently to defaults when the file is absent or
    // unreadable. Parse errors on individual lines log to stderr but do
    // not fail the process — a broken config shouldn't stop the user
    // from attaching.
    pub fn load() -> Self {
        let path = match config_path() {
            Some(p) => p,
            None => return Self::default(),
        };
        let Ok(content) = fs::read_to_string(&path) else {
            return Self::default();
        };
        Self::parse_str_with_path(&content, Some(&path))
    }

    /// Parse a config from raw text. Used by tests so we don't have to
    /// touch the filesystem; also the seam through which `load` runs.
    /// Errors on individual lines are logged to stderr (when `path` is
    /// Some) and otherwise silently ignored — a broken config should
    /// never fail the process.
    // Infallible by design (broken lines are skipped), so this can't
    // impl `FromStr` (which requires a `Result`). Keep the name.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(content: &str) -> Self {
        Self::parse_str_with_path(content, None)
    }

    fn parse_str_with_path(content: &str, path: Option<&Path>) -> Self {
        let mut config = Self::default();
        let mut section: Option<String> = None;
        for (index, raw) in content.lines().enumerate() {
            let stripped = match raw.find('#') {
                Some(position) => &raw[..position],
                None => raw,
            };
            let line = stripped.trim();
            if line.is_empty() {
                continue;
            }
            // Section header: `[name]`.
            if let Some(rest) = line.strip_prefix('[')
                && let Some(name) = rest.strip_suffix(']')
            {
                section = Some(name.trim().to_string());
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                report(path, index + 1, "expected `key = value` (or `[section]`)");
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match section.as_deref() {
                Some("agent") => apply_agent_entry(&mut config.agent, key, value, path, index + 1),
                _ => apply_entry(&mut config, key, value, path, index + 1),
            }
        }
        config
    }
}

fn report(path: Option<&Path>, line: usize, message: &str) {
    match path {
        Some(p) => eprintln!("zmux: config {}:{}: {message}", p.display(), line),
        None => {
            // Test mode: stay silent so cargo output isn't polluted.
        }
    }
}

fn apply_entry(config: &mut Config, key: &str, value: &str, path: Option<&Path>, line: usize) {
    match key {
        "prefix" => {
            if let Some(byte) = parse_prefix(value) {
                config.prefix_byte = byte;
            } else {
                report(
                    path,
                    line,
                    &format!("unrecognized prefix `{value}` (expected \"ctrl-<letter>\")"),
                );
            }
        }
        "scrollback" => match value.parse::<usize>() {
            Ok(lines) if lines > 0 => config.scrollback_lines = lines,
            _ => report(path, line, "scrollback must be a positive integer"),
        },
        "status_hints" => match value {
            "true" => config.status_bar_hints = true,
            "false" => config.status_bar_hints = false,
            _ => report(path, line, "status_hints must be `true` or `false`"),
        },
        "status_label" => {
            let stripped = value.trim_matches('"').trim_matches('\'');
            config.status_label = if stripped.is_empty() {
                None
            } else {
                Some(stripped.to_string())
            };
        }
        _ => {
            // Unknown keys are silently tolerated so newer configs stay
            // loadable on older binaries.
        }
    }
}

fn apply_agent_entry(
    agent: &mut AgentConfig,
    key: &str,
    value: &str,
    path: Option<&Path>,
    line: usize,
) {
    match key {
        "idle_threshold_ms" => match value.parse::<u64>() {
            Ok(ms) => agent.idle_threshold_ms = ms,
            Err(_) => report(
                path,
                line,
                "idle_threshold_ms must be a non-negative integer",
            ),
        },
        "shell_prompts" => match parse_string_array(value) {
            Some(list) => agent.shell_prompts = list,
            None => report(
                path,
                line,
                "shell_prompts must be an array of quoted strings",
            ),
        },
        "agent_prompts" => match parse_string_array(value) {
            Some(list) => agent.agent_prompts = list,
            None => report(
                path,
                line,
                "agent_prompts must be an array of quoted strings",
            ),
        },
        _ => {
            // Unknown agent keys are tolerated for forward compat.
        }
    }
}

/// Parse a JSON-style array literal of double- or single-quoted strings.
/// Returns None on malformed input. Empty arrays (`[]`) are valid and
/// parse to `Some(vec![])`. Whitespace is permitted between tokens.
fn parse_string_array(raw: &str) -> Option<Vec<String>> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    let mut iter = inner.chars().peekable();
    loop {
        // Skip whitespace and trailing commas.
        while let Some(&c) = iter.peek() {
            if c.is_whitespace() || c == ',' {
                iter.next();
            } else {
                break;
            }
        }
        let Some(&first) = iter.peek() else {
            return Some(out);
        };
        if first != '"' && first != '\'' {
            // Unquoted token isn't supported.
            return None;
        }
        let quote = first;
        iter.next();
        let mut buf = String::new();
        let mut closed = false;
        while let Some(c) = iter.next() {
            if c == '\\' {
                // Minimal escape handling: passthrough next char.
                if let Some(esc) = iter.next() {
                    buf.push(esc);
                }
                continue;
            }
            if c == quote {
                closed = true;
                break;
            }
            buf.push(c);
        }
        if !closed {
            return None;
        }
        out.push(buf);
    }
}

// Accepts `"ctrl-a"` through `"ctrl-z"` (quoted or bare, any case). Any
// other spelling returns None and the caller falls back to the default
// prefix. The byte produced is the standard control code: C-a = 0x01,
// C-b = 0x02, ... C-z = 0x1A.
fn parse_prefix(raw: &str) -> Option<u8> {
    let stripped = raw.trim_matches('"').trim_matches('\'');
    let lower = stripped.to_ascii_lowercase();
    let rest = lower.strip_prefix("ctrl-")?;
    let byte = rest.as_bytes();
    if byte.len() != 1 {
        return None;
    }
    let ch = byte[0];
    if !ch.is_ascii_lowercase() {
        return None;
    }
    Some(ch - b'a' + 1)
}

fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("zmux").join("config.toml"));
    }
    if let Ok(home) = env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join(".config/zmux/config.toml"));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::sync::Mutex;

    use super::{Config, parse_prefix, parse_string_array};

    // XDG_CONFIG_HOME is a process-wide env var; tests that mutate it
    // must serialize so one test's write doesn't leak into another's
    // read. A plain Mutex guards the critical section.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn defaults_are_sensible() {
        let config = Config::default();
        assert_eq!(config.prefix_byte, 0x01);
        assert_eq!(config.scrollback_lines, 8_192);
        assert!(config.status_bar_hints);
        assert_eq!(config.agent.idle_threshold_ms, 750);
        assert!(!config.agent.shell_prompts.is_empty());
        assert!(!config.agent.agent_prompts.is_empty());
    }

    #[test]
    fn parse_prefix_recognizes_ctrl_letter_forms() {
        assert_eq!(parse_prefix("\"ctrl-a\""), Some(0x01));
        assert_eq!(parse_prefix("ctrl-b"), Some(0x02));
        assert_eq!(parse_prefix("Ctrl-Z"), Some(0x1A));
        assert_eq!(parse_prefix("CTRL-s"), Some(0x13));
    }

    #[test]
    fn load_reads_a_full_config_file_from_xdg_config_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = env::temp_dir().join(format!("zmux-config-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let zmux_dir = dir.join("zmux");
        fs::create_dir_all(&zmux_dir).expect("mk zmux dir");
        fs::write(
            zmux_dir.join("config.toml"),
            b"# comment line\n\
              prefix = \"ctrl-s\"   # trailing comment\n\
              scrollback = 16384\n\
              status_hints = false\n\
              unknown_key = \"ignored\"\n",
        )
        .expect("write config");

        let prior = env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: set_var is unsafe in 2024 edition because env mutation
        // is racy. ENV_LOCK above serializes the tests that touch it.
        unsafe {
            env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let config = Config::load();
        unsafe {
            match prior {
                Some(value) => env::set_var("XDG_CONFIG_HOME", value),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(config.prefix_byte, 0x13); // Ctrl-s
        assert_eq!(config.scrollback_lines, 16_384);
        assert!(!config.status_bar_hints);
    }

    #[test]
    fn parse_prefix_rejects_garbage() {
        assert_eq!(parse_prefix("alt-a"), None);
        assert_eq!(parse_prefix("ctrl-"), None);
        assert_eq!(parse_prefix("ctrl-ab"), None);
        assert_eq!(parse_prefix("ctrl-1"), None);
        assert_eq!(parse_prefix(""), None);
    }

    #[test]
    fn agent_config_overrides_defaults_from_toml() {
        let toml = r#"
            [agent]
            idle_threshold_ms = 250
            shell_prompts = ["$> "]
        "#;
        let cfg = Config::from_str(toml);
        assert_eq!(cfg.agent.idle_threshold_ms, 250);
        assert_eq!(cfg.agent.shell_prompts, vec!["$> ".to_string()]);
        // agent_prompts unaffected — defaults preserved.
        assert!(!cfg.agent.agent_prompts.is_empty());
    }

    #[test]
    fn agent_section_top_level_keys_still_parse() {
        // Mixed section + flat keys.
        let toml = "\
            prefix = \"ctrl-b\"\n\
            [agent]\n\
            idle_threshold_ms = 100\n\
        ";
        let cfg = Config::from_str(toml);
        assert_eq!(cfg.prefix_byte, 0x02);
        assert_eq!(cfg.agent.idle_threshold_ms, 100);
    }

    #[test]
    fn parse_string_array_handles_mixed_inputs() {
        assert_eq!(parse_string_array("[]"), Some(Vec::<String>::new()));
        assert_eq!(parse_string_array("[\"a\"]"), Some(vec!["a".to_string()]));
        assert_eq!(
            parse_string_array("[\"a\", \"b\", \"c\"]"),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        // Single-quoted variant.
        assert_eq!(parse_string_array("['a']"), Some(vec!["a".to_string()]));
        // Whitespace-only inner.
        assert_eq!(parse_string_array("[   ]"), Some(Vec::<String>::new()));
        // Garbage.
        assert_eq!(parse_string_array("not-an-array"), None);
        assert_eq!(parse_string_array("[unquoted]"), None);
    }
}
