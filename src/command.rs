// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Word(String),
    Separator, // `;` between commands
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: String,
    pub position: usize,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at column {}", self.message, self.position + 1)
    }
}

pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b' ' || c == b'\t' || c == b'\n' {
            i += 1;
            continue;
        }
        // A bare `#` at a token boundary starts a comment, but `#{`
        // introduces a format placeholder — let the word scanner pick
        // that up so we don't swallow it.
        if c == b'#' && !(i + 1 < bytes.len() && bytes[i + 1] == b'{') {
            // Comment to end of line — consume rest of input.
            break;
        }
        if c == b';' {
            tokens.push(Token::Separator);
            i += 1;
            continue;
        }
        // Word (possibly containing quoted regions and #{...} placeholders).
        let mut buf = String::new();
        while i < bytes.len() {
            let c = bytes[i];
            match c {
                b' ' | b'\t' | b'\n' | b';' => break,
                b'#' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
                    // Format placeholder — copy verbatim through the
                    // closing `}` so the dispatcher can expand later.
                    let ph_start = i;
                    i += 2; // consume `#{`
                    while i < bytes.len() && bytes[i] != b'}' {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return Err(LexError {
                            message: "unterminated format placeholder".into(),
                            position: ph_start,
                        });
                    }
                    i += 1; // consume `}`
                    buf.push_str(&input[ph_start..i]);
                }
                b'#' => {
                    // `#` not followed by `{` after content has already
                    // been accumulated: treat it as literal so tokens like
                    // `foo#bar` survive. Comment-start is only recognised
                    // at a token boundary, which is handled by the outer
                    // loop before we get here.
                    buf.push('#');
                    i += 1;
                }
                b'"' => {
                    i += 1;
                    let quote_open = i - 1;
                    let mut chunk_start = i;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            // Flush the verbatim run up to the backslash
                            // before translating the escape sequence.
                            if chunk_start < i {
                                buf.push_str(&input[chunk_start..i]);
                            }
                            let esc = bytes[i + 1];
                            let ch = match esc {
                                b'n' => '\n',
                                b't' => '\t',
                                b'\\' => '\\',
                                b'"' => '"',
                                other => {
                                    return Err(LexError {
                                        message: format!("unknown escape `\\{}`", other as char),
                                        position: i,
                                    });
                                }
                            };
                            buf.push(ch);
                            i += 2;
                            chunk_start = i;
                        } else {
                            i += 1;
                        }
                    }
                    if i >= bytes.len() {
                        return Err(LexError {
                            message: "unterminated double-quoted string".into(),
                            position: quote_open,
                        });
                    }
                    // Flush any trailing verbatim run before the closing quote.
                    if chunk_start < i {
                        buf.push_str(&input[chunk_start..i]);
                    }
                    i += 1; // consume closing `"`
                }
                b'\'' => {
                    i += 1;
                    let quote_open = i - 1;
                    let chunk_start = i;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return Err(LexError {
                            message: "unterminated single-quoted string".into(),
                            position: quote_open,
                        });
                    }
                    // Single quotes have no escapes — copy the run verbatim.
                    buf.push_str(&input[chunk_start..i]);
                    i += 1; // consume closing `'`
                }
                _ => {
                    // Bareword run of literal bytes. All sentinels above
                    // are ASCII, so scanning byte-by-byte keeps us on
                    // UTF-8 char boundaries and we can slice the original
                    // `&str` directly.
                    let chunk_start = i;
                    while i < bytes.len() {
                        match bytes[i] {
                            b' ' | b'\t' | b'\n' | b';' | b'"' | b'\'' => break,
                            b'#' => break, // let the outer match decide
                            _ => i += 1,
                        }
                    }
                    buf.push_str(&input[chunk_start..i]);
                }
            }
        }
        tokens.push(Token::Word(buf));
    }
    Ok(tokens)
}

#[cfg(test)]
mod lexer_tests {
    use super::*;

    #[test]
    fn tokenizes_bareword() {
        assert_eq!(
            tokenize("split-window").unwrap(),
            vec![Token::Word("split-window".into())],
        );
    }

    #[test]
    fn whitespace_splits_tokens() {
        assert_eq!(
            tokenize("kill-pane  -t   3").unwrap(),
            vec![
                Token::Word("kill-pane".into()),
                Token::Word("-t".into()),
                Token::Word("3".into()),
            ],
        );
    }

    #[test]
    fn double_quotes_group_tokens_and_expand_escapes() {
        assert_eq!(
            tokenize(r#"display-message "hello world" "line\nbreak""#).unwrap(),
            vec![
                Token::Word("display-message".into()),
                Token::Word("hello world".into()),
                Token::Word("line\nbreak".into()),
            ],
        );
    }

    #[test]
    fn single_quotes_group_literally() {
        assert_eq!(
            tokenize(r"rename-window 'foo\nbar'").unwrap(),
            vec![
                Token::Word("rename-window".into()),
                Token::Word(r"foo\nbar".into()),
            ],
        );
    }

    #[test]
    fn semicolon_separates_commands() {
        assert_eq!(
            tokenize("split-window -h ; resize-pane -D 5").unwrap(),
            vec![
                Token::Word("split-window".into()),
                Token::Word("-h".into()),
                Token::Separator,
                Token::Word("resize-pane".into()),
                Token::Word("-D".into()),
                Token::Word("5".into()),
            ],
        );
    }

    #[test]
    fn hash_starts_comment_outside_quotes() {
        assert_eq!(
            tokenize("split-window -h # a comment").unwrap(),
            vec![Token::Word("split-window".into()), Token::Word("-h".into()),],
        );
    }

    #[test]
    fn format_placeholder_is_its_own_token() {
        assert_eq!(
            tokenize(r#"display-message "pane #{pane_index}""#).unwrap(),
            vec![
                Token::Word("display-message".into()),
                // The lexer leaves #{...} inside the surrounding word so
                // the dispatcher can expand it in place.
                Token::Word("pane #{pane_index}".into()),
            ],
        );
    }

    #[test]
    fn bare_format_placeholder_is_not_swallowed_as_comment() {
        assert_eq!(
            tokenize("display-message #{pane_index}").unwrap(),
            vec![
                Token::Word("display-message".into()),
                Token::Word("#{pane_index}".into()),
            ],
        );
    }

    #[test]
    fn non_ascii_content_round_trips_through_words() {
        // UTF-8 multibyte bytes must survive the lexer unchanged,
        // whether they appear bare, inside double quotes, or inside
        // single quotes.
        assert_eq!(
            tokenize("rename-window café").unwrap(),
            vec![
                Token::Word("rename-window".into()),
                Token::Word("café".into()),
            ],
        );
        assert_eq!(
            tokenize(r#"display-message "naïve 🚀""#).unwrap(),
            vec![
                Token::Word("display-message".into()),
                Token::Word("naïve 🚀".into()),
            ],
        );
        assert_eq!(
            tokenize("rename-window 'niño'").unwrap(),
            vec![
                Token::Word("rename-window".into()),
                Token::Word("niño".into()),
            ],
        );
    }

    #[test]
    fn unterminated_double_quote_is_an_error() {
        assert!(tokenize(r#"display-message "oops"#).is_err());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub name: char,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub flags: Vec<Flag>,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError {
            message: e.to_string(),
        }
    }
}

// Which short flags expect a following value token. The set is
// per-command because the same letter means different things in
// different contexts — e.g. `-n` is the name on new-window but
// "next" on select-window; `-l` is "last" on select-pane/window.
// A global set would force parse errors on boolean uses, so we
// disambiguate by the command being parsed. Commands that take
// their name as a positional arg (rename-window, rename-session)
// don't belong here — registering `-n` would silently swallow the
// name into the flag value.
fn flag_takes_value(command: &str, flag: char) -> bool {
    match (command, flag) {
        // Commands where -n carries a name:
        ("new-window", 'n') => true,
        // Commands where -n / -p / -l are boolean directional flags:
        ("select-window", 'n') | ("select-window", 'p') | ("select-window", 'l') => false,
        ("select-pane", 'l') => false,
        // -t is always a target value, across all commands.
        (_, 't') => true,
        // -c (cwd), -s (source), -F (format) are always value-taking.
        (_, 'c') | (_, 's') | (_, 'F') => true,
        // Default: boolean flag.
        _ => false,
    }
}

pub fn parse(input: &str) -> Result<Vec<Command>, ParseError> {
    let tokens = tokenize(input)?;
    let mut commands = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        // Skip leading separators.
        while i < tokens.len() && tokens[i] == Token::Separator {
            i += 1;
        }
        if i >= tokens.len() {
            break;
        }
        // First word is the command name.
        let name = match &tokens[i] {
            Token::Word(w) if !w.starts_with('-') => w.clone(),
            Token::Word(w) => {
                return Err(ParseError {
                    message: format!("expected command name, got flag `{w}`"),
                });
            }
            Token::Separator => unreachable!("skipped above"),
        };
        i += 1;

        let mut flags = Vec::new();
        let mut args = Vec::new();
        while i < tokens.len() && tokens[i] != Token::Separator {
            let word = match &tokens[i] {
                Token::Word(w) => w.clone(),
                Token::Separator => unreachable!(),
            };
            if let Some(rest) = word.strip_prefix('-') {
                if rest.is_empty() {
                    return Err(ParseError {
                        message: "empty flag: `-`".into(),
                    });
                }
                if rest.starts_with('-') {
                    return Err(ParseError {
                        message: format!("long flags not supported: `{word}`"),
                    });
                }
                if rest.chars().count() != 1 {
                    return Err(ParseError {
                        message: format!("bundled flags not supported: `{word}`"),
                    });
                }
                // Guaranteed Some by the count==1 check above.
                let flag_name = rest.chars().next().unwrap();
                if flag_takes_value(&name, flag_name) {
                    i += 1;
                    let value = match tokens.get(i) {
                        Some(Token::Word(v)) => v.clone(),
                        _ => {
                            return Err(ParseError {
                                message: format!("flag -{flag_name} requires a value"),
                            });
                        }
                    };
                    flags.push(Flag {
                        name: flag_name,
                        value: Some(value),
                    });
                    i += 1;
                } else {
                    flags.push(Flag {
                        name: flag_name,
                        value: None,
                    });
                    i += 1;
                }
            } else {
                args.push(word);
                i += 1;
            }
        }
        commands.push(Command { name, flags, args });
    }
    Ok(commands)
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn parses_bare_command() {
        let parsed = parse("detach-client").unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "detach-client");
        assert!(parsed[0].flags.is_empty());
        assert!(parsed[0].args.is_empty());
    }

    #[test]
    fn parses_flags_with_values() {
        let parsed = parse("kill-pane -t 3").unwrap();
        assert_eq!(
            parsed[0].flags,
            vec![Flag {
                name: 't',
                value: Some("3".into())
            }]
        );
    }

    #[test]
    fn parses_boolean_flag() {
        let parsed = parse("split-window -h").unwrap();
        assert_eq!(
            parsed[0].flags,
            vec![Flag {
                name: 'h',
                value: None
            }]
        );
    }

    #[test]
    fn parses_trailing_args() {
        let parsed = parse(r#"display-message "hello world""#).unwrap();
        assert_eq!(parsed[0].args, vec!["hello world".to_string()]);
    }

    #[test]
    fn parses_multiple_commands() {
        let parsed = parse("split-window -h ; resize-pane -D 5").unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "split-window");
        assert_eq!(parsed[1].name, "resize-pane");
    }

    #[test]
    fn empty_input_parses_to_empty_list() {
        assert_eq!(parse("").unwrap(), vec![]);
        assert_eq!(parse("  # just a comment").unwrap(), vec![]);
    }

    #[test]
    fn rejects_unknown_flag_shape() {
        // Long flags are not supported in v1.
        assert!(parse("split-window --horizontal").is_err());
    }

    #[test]
    fn rejects_leading_flag() {
        assert!(parse("-t 3").is_err());
    }

    #[test]
    fn flag_expecting_value_at_eof_errors() {
        assert!(parse("kill-pane -t").is_err());
    }

    #[test]
    fn multiple_flags_parse_in_sequence() {
        let parsed = parse("resize-pane -U -D").unwrap();
        assert_eq!(
            parsed[0].flags,
            vec![
                Flag {
                    name: 'U',
                    value: None
                },
                Flag {
                    name: 'D',
                    value: None
                },
            ]
        );
    }

    #[test]
    fn bundled_short_flags_are_rejected() {
        assert!(parse("cmd -hv").is_err());
    }

    #[test]
    fn flag_value_may_start_with_dash() {
        let parsed = parse("kill-pane -t -1").unwrap();
        assert_eq!(
            parsed[0].flags,
            vec![Flag {
                name: 't',
                value: Some("-1".into())
            }]
        );
    }

    #[test]
    fn select_window_n_parses_as_boolean() {
        // Regression: -n is a name-flag for new-window but boolean for select-window.
        let parsed = parse("select-window -n").unwrap();
        assert_eq!(
            parsed[0].flags,
            vec![Flag {
                name: 'n',
                value: None
            }]
        );
    }

    #[test]
    fn new_window_n_parses_with_name_value() {
        let parsed = parse("new-window -n build").unwrap();
        assert_eq!(
            parsed[0].flags,
            vec![Flag {
                name: 'n',
                value: Some("build".into())
            }]
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRef {
    Index(u32),
    Name(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeTarget {
    Last,
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSpec {
    PaneId(u32),
    Window {
        session: Option<String>,
        window: WindowRef,
    },
    Pane {
        session: Option<String>,
        window: WindowRef,
        pane: u32,
    },
    Relative(RelativeTarget),
}

pub fn parse_target(raw: &str) -> Result<TargetSpec, ParseError> {
    if raw.is_empty() {
        return Err(ParseError {
            message: "empty target spec".into(),
        });
    }
    if let Some(rest) = raw.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        return match rest {
            "last" => Ok(TargetSpec::Relative(RelativeTarget::Last)),
            "next" => Ok(TargetSpec::Relative(RelativeTarget::Next)),
            "prev" => Ok(TargetSpec::Relative(RelativeTarget::Prev)),
            other => Err(ParseError {
                message: format!("unknown relative target `{{{other}}}`"),
            }),
        };
    }
    if !raw.contains(':') && !raw.contains('.') {
        return match raw.parse::<u32>() {
            Ok(id) => Ok(TargetSpec::PaneId(id)),
            Err(_) => Err(ParseError {
                message: format!("expected pane id, got `{raw}`"),
            }),
        };
    }
    // full form: [session]:window[.pane]
    let (session, rest) = match raw.split_once(':') {
        Some(("", r)) => (None, r),
        Some((s, r)) => (Some(s.to_string()), r),
        None => (None, raw),
    };
    if let Some((w, p)) = rest.split_once('.') {
        let pane = p.parse::<u32>().map_err(|_| ParseError {
            message: format!("expected pane index, got `{p}`"),
        })?;
        Ok(TargetSpec::Pane {
            session,
            window: parse_window_ref(w)?,
            pane,
        })
    } else {
        Ok(TargetSpec::Window {
            session,
            window: parse_window_ref(rest)?,
        })
    }
}

fn parse_window_ref(raw: &str) -> Result<WindowRef, ParseError> {
    if raw.is_empty() {
        return Err(ParseError {
            message: "empty window ref".into(),
        });
    }
    match raw.parse::<u32>() {
        Ok(idx) => Ok(WindowRef::Index(idx)),
        Err(_) => Ok(WindowRef::Name(raw.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Format placeholder expansion  (#{name} → value)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FormatContext {
    pub session_name: String,
    pub session_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub window_id: String,
    pub pane_index: u32,
    pub pane_id: String,
    pub pane_current_command: String,
    pub host: String,
    pub host_short: String,
    pub user: String,
}

fn lookup(ctx: &FormatContext, name: &str) -> String {
    match name {
        "session_name" => ctx.session_name.clone(),
        "session_id" => ctx.session_id.clone(),
        "window_index" => ctx.window_index.to_string(),
        "window_name" => ctx.window_name.clone(),
        "window_id" => ctx.window_id.clone(),
        "pane_index" => ctx.pane_index.to_string(),
        "pane_id" => ctx.pane_id.clone(),
        "pane_current_command" => ctx.pane_current_command.clone(),
        "host" => ctx.host.clone(),
        "host_short" => ctx.host_short.clone(),
        "user" => ctx.user.clone(),
        _ => String::new(),
    }
}

/// Expand `#{name}` placeholders in `input` using values from `ctx`.
///
/// Unknown placeholders expand to the empty string, matching tmux behaviour.
/// Unterminated `#{` sequences are passed through literally.
pub fn expand_format(input: &str, ctx: &FormatContext) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut chunk_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'#' && bytes[i + 1] == b'{' {
            // Flush the run of literal text before the placeholder.
            if chunk_start < i {
                out.push_str(&input[chunk_start..i]);
            }
            let name_start = i + 2;
            let Some(end_offset) = input[name_start..].find('}') else {
                // Unterminated — flush the rest literally.
                out.push_str(&input[i..]);
                return out;
            };
            let name_end = name_start + end_offset;
            let name = &input[name_start..name_end];
            out.push_str(&lookup(ctx, name));
            i = name_end + 1;
            chunk_start = i;
        } else {
            i += 1;
        }
    }
    // Flush any remaining literal text.
    if chunk_start < bytes.len() {
        out.push_str(&input[chunk_start..]);
    }
    out
}

#[cfg(test)]
mod target_tests {
    use super::*;

    #[test]
    fn parses_bare_pane_id() {
        assert_eq!(parse_target("3").unwrap(), TargetSpec::PaneId(3));
    }

    #[test]
    fn parses_window_only() {
        assert_eq!(
            parse_target(":2").unwrap(),
            TargetSpec::Window {
                session: None,
                window: WindowRef::Index(2)
            }
        );
        assert_eq!(
            parse_target(":dev").unwrap(),
            TargetSpec::Window {
                session: None,
                window: WindowRef::Name("dev".into())
            }
        );
    }

    #[test]
    fn parses_full_spec() {
        assert_eq!(
            parse_target("work:2.1").unwrap(),
            TargetSpec::Pane {
                session: Some("work".into()),
                window: WindowRef::Index(2),
                pane: 1,
            },
        );
    }

    #[test]
    fn parses_relative_tokens() {
        assert_eq!(
            parse_target("{last}").unwrap(),
            TargetSpec::Relative(RelativeTarget::Last)
        );
        assert_eq!(
            parse_target("{next}").unwrap(),
            TargetSpec::Relative(RelativeTarget::Next)
        );
        assert_eq!(
            parse_target("{prev}").unwrap(),
            TargetSpec::Relative(RelativeTarget::Prev)
        );
    }

    #[test]
    fn rejects_unknown_relative() {
        assert!(parse_target("{mouse}").is_err());
    }

    #[test]
    fn empty_string_is_an_error() {
        assert!(parse_target("").is_err());
    }

    #[test]
    fn parses_window_dot_pane_with_implicit_session() {
        assert_eq!(
            parse_target(":1.2").unwrap(),
            TargetSpec::Pane {
                session: None,
                window: WindowRef::Index(1),
                pane: 2,
            },
        );
    }

    #[test]
    fn rejects_empty_window_in_full_form() {
        // `a:` has session "a" but nothing after the colon.
        assert!(parse_target("a:").is_err());
    }

    #[test]
    fn session_with_dot_still_routes_dot_to_pane() {
        // A `.` after the `:` separates window from pane regardless of dots in the session name.
        // "my.session:1.2" → session="my.session", window=Index(1), pane=2
        assert_eq!(
            parse_target("my.session:1.2").unwrap(),
            TargetSpec::Pane {
                session: Some("my.session".into()),
                window: WindowRef::Index(1),
                pane: 2,
            },
        );
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    fn ctx() -> FormatContext {
        FormatContext {
            session_name: "work".into(),
            session_id: "$1".into(),
            window_index: 2,
            window_name: "dev".into(),
            window_id: "@5".into(),
            pane_index: 1,
            pane_id: "%3".into(),
            pane_current_command: "bash".into(),
            host: "tower.local".into(),
            host_short: "tower".into(),
            user: "zach".into(),
        }
    }

    #[test]
    fn expands_known_placeholders() {
        assert_eq!(expand_format("pane #{pane_index}", &ctx()), "pane 1");
        assert_eq!(expand_format("#{user}@#{host_short}", &ctx()), "zach@tower");
    }

    #[test]
    fn leaves_text_without_placeholders_alone() {
        assert_eq!(
            expand_format("no placeholders here", &ctx()),
            "no placeholders here"
        );
    }

    #[test]
    fn unknown_placeholder_renders_as_empty() {
        // tmux renders unknown vars as empty. We follow that.
        assert_eq!(expand_format("x=#{not_a_var}y", &ctx()), "x=y");
    }

    #[test]
    fn handles_multiple_placeholders_in_one_string() {
        assert_eq!(
            expand_format("#{session_name}:#{window_index}.#{pane_index}", &ctx()),
            "work:2.1",
        );
    }

    #[test]
    fn preserves_non_ascii_outside_placeholders() {
        assert_eq!(
            expand_format("café #{user} naïve 🚀", &ctx()),
            "café zach naïve 🚀",
        );
    }

    #[test]
    fn unterminated_placeholder_passes_through_literally() {
        assert_eq!(
            expand_format("prefix #{unterminated", &ctx()),
            "prefix #{unterminated"
        );
    }

    #[test]
    fn nested_looking_placeholder_is_not_recursive() {
        // `#{#{foo}}` → name is "#{foo" (unknown) → empty, trailing `}` literal.
        // Intentional non-recursive behavior; tmux is recursive but Tier 1 is not.
        assert_eq!(expand_format("#{#{foo}}", &ctx()), "}");
    }

    #[test]
    fn empty_placeholder_name_expands_to_empty() {
        // `#{}` looks up the empty name, which matches no known variable.
        assert_eq!(expand_format("#{}", &ctx()), "");
    }
}
