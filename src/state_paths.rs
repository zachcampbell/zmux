// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::env;
use std::path::PathBuf;

pub fn state_dir() -> PathBuf {
    if let Ok(path) = env::var("ZMUX_STATE_DIR")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    if let Ok(path) = env::var("XDG_STATE_HOME")
        && !path.is_empty()
    {
        return PathBuf::from(path).join("zmux");
    }
    if let Ok(home) = env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".local/state/zmux");
    }
    env::temp_dir().join("zmux-state")
}

pub fn safe_component(input: &str) -> String {
    if input.is_empty() {
        return "_".into();
    }
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("_{byte:02x}")),
        }
    }
    out
}

pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_component_keeps_simple_names() {
        assert_eq!(safe_component("default.worker-1"), "default.worker-1");
    }

    #[test]
    fn safe_component_escapes_path_like_names() {
        assert_eq!(safe_component("a/b c"), "a_2fb_20c");
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("it's fine"), "'it'\\''s fine'");
    }
}
