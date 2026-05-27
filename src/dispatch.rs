// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::command::{Command, FormatContext, expand_format};
use crate::layout::PaneId;
use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct ClientId(pub u32);

pub struct CommandContext<'a> {
    pub workspace: &'a mut Workspace,
    pub caller_pane: PaneId,
    pub caller_client: ClientId,
    pub format: FormatContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffect {
    None,
    Detach,
    ShutdownSession,
    DisplayMessage(String),
    SwitchSession(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub side_effect: SideEffect,
}

impl CommandOutput {
    pub fn none() -> Self {
        Self {
            side_effect: SideEffect::None,
        }
    }
    pub fn display(message: impl Into<String>) -> Self {
        Self {
            side_effect: SideEffect::DisplayMessage(message.into()),
        }
    }
}

pub type Handler = fn(&Command, &mut CommandContext) -> Result<CommandOutput, String>;

pub struct HandlerSpec {
    pub name: &'static str,
    pub handler: Handler,
}

pub struct CommandRegistry {
    handlers: Vec<HandlerSpec>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }
    pub fn register(&mut self, spec: HandlerSpec) {
        self.handlers.push(spec);
    }
    pub fn get(&self, name: &str) -> Option<&HandlerSpec> {
        self.handlers.iter().find(|h| h.name == name)
    }
    pub fn names(&self) -> Vec<&'static str> {
        self.handlers.iter().map(|h| h.name).collect()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub fn dispatch(
    cmd: &Command,
    registry: &CommandRegistry,
    ctx: &mut CommandContext,
) -> Result<CommandOutput, String> {
    let Some(spec) = registry.get(&cmd.name) else {
        return Err(format!("unknown command: {}", cmd.name));
    };
    // Expand format placeholders in every arg and every flag value before
    // handing off, so handlers get fully-resolved strings.
    let expanded_args: Vec<String> = cmd
        .args
        .iter()
        .map(|a| expand_format(a, &ctx.format))
        .collect();
    let mut expanded_flags = cmd.flags.clone();
    for f in expanded_flags.iter_mut() {
        if let Some(v) = f.value.as_mut() {
            *v = expand_format(v, &ctx.format);
        }
    }
    let expanded = Command {
        name: cmd.name.clone(),
        flags: expanded_flags,
        args: expanded_args,
    };
    (spec.handler)(&expanded, ctx)
}

fn handle_display_message(
    cmd: &Command,
    _ctx: &mut CommandContext,
) -> Result<CommandOutput, String> {
    let message = cmd
        .args
        .first()
        .ok_or_else(|| "display-message requires a message argument".to_string())?
        .clone();
    Ok(CommandOutput::display(message))
}

pub fn register_tier1(registry: &mut CommandRegistry) {
    registry.register(HandlerSpec {
        name: "display-message",
        handler: handle_display_message,
    });
    // Additional handlers registered in later tasks.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_format() -> FormatContext {
        FormatContext {
            session_name: "".into(),
            session_id: "".into(),
            window_index: 0,
            window_name: "".into(),
            window_id: "".into(),
            pane_index: 0,
            pane_id: "".into(),
            pane_current_command: "".into(),
            host: "".into(),
            host_short: "".into(),
            user: "alice".into(),
        }
    }

    fn noop(_: &Command, _: &mut CommandContext) -> Result<CommandOutput, String> {
        Ok(CommandOutput::none())
    }

    #[test]
    fn registry_lookup() {
        let mut reg = CommandRegistry::new();
        reg.register(HandlerSpec {
            name: "foo",
            handler: noop,
        });
        assert!(reg.get("foo").is_some());
        assert!(reg.get("bar").is_none());
    }

    #[test]
    fn registry_names_lists_registered_handlers() {
        let mut reg = CommandRegistry::new();
        reg.register(HandlerSpec {
            name: "a",
            handler: noop,
        });
        reg.register(HandlerSpec {
            name: "b",
            handler: noop,
        });
        let names = reg.names();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn command_output_helpers() {
        let none = CommandOutput::none();
        assert_eq!(none.side_effect, SideEffect::None);

        let msg = CommandOutput::display("hi");
        assert_eq!(msg.side_effect, SideEffect::DisplayMessage("hi".into()));
    }

    #[test]
    fn dispatch_unknown_command_returns_err() {
        use crate::command::parse;
        use crate::pty::PtySize;
        use crate::workspace::Workspace;

        let reg = CommandRegistry::new();
        let cmd = parse("no-such-command").unwrap().remove(0);
        let mut ws = Workspace::spawn_single("/bin/sh", PtySize { rows: 24, cols: 80 }).unwrap();
        let mut ctx = CommandContext {
            workspace: &mut ws,
            caller_pane: 0,
            caller_client: ClientId(0),
            format: test_format(),
        };
        let err = dispatch(&cmd, &reg, &mut ctx).unwrap_err();
        assert!(err.contains("unknown command"));
    }

    #[test]
    fn dispatch_expands_format_placeholders_before_handler_sees_them() {
        use crate::command::parse;
        use crate::pty::PtySize;
        use crate::workspace::Workspace;
        use std::cell::RefCell;

        // Handler captures the arg it received via a thread-local so we can
        // read it after dispatch returns. fn pointers can't capture state, so
        // we stash in a thread-local.
        thread_local! {
            static CAPTURED: RefCell<Option<String>> = const { RefCell::new(None) };
        }
        fn capture(cmd: &Command, _: &mut CommandContext) -> Result<CommandOutput, String> {
            CAPTURED.with(|c| *c.borrow_mut() = cmd.args.first().cloned());
            Ok(CommandOutput::none())
        }

        let mut reg = CommandRegistry::new();
        reg.register(HandlerSpec {
            name: "capture",
            handler: capture,
        });
        let cmd = parse(r#"capture "hello #{user}""#).unwrap().remove(0);
        let mut ws = Workspace::spawn_single("/bin/sh", PtySize { rows: 24, cols: 80 }).unwrap();
        let mut ctx = CommandContext {
            workspace: &mut ws,
            caller_pane: 0,
            caller_client: ClientId(0),
            format: test_format(),
        };
        dispatch(&cmd, &reg, &mut ctx).unwrap();
        CAPTURED.with(|c| {
            assert_eq!(c.borrow().as_deref(), Some("hello alice"));
        });
    }
}

#[cfg(test)]
mod display_message_tests {
    use super::*;
    use crate::command::parse;
    use crate::pty::PtySize;
    use crate::workspace::Workspace;

    #[test]
    fn display_message_returns_display_side_effect() {
        let mut reg = CommandRegistry::new();
        register_tier1(&mut reg);
        let cmd = parse(r#"display-message "hello #{user}""#)
            .unwrap()
            .remove(0);
        let mut ws = Workspace::spawn_single("/bin/sh", PtySize { rows: 24, cols: 80 }).unwrap();
        let mut ctx = CommandContext {
            workspace: &mut ws,
            caller_pane: 0,
            caller_client: ClientId(0),
            format: FormatContext {
                session_name: "".into(),
                session_id: "".into(),
                window_index: 0,
                window_name: "".into(),
                window_id: "".into(),
                pane_index: 0,
                pane_id: "".into(),
                pane_current_command: "".into(),
                host: "".into(),
                host_short: "".into(),
                user: "zach".into(),
            },
        };
        let out = dispatch(&cmd, &reg, &mut ctx).unwrap();
        assert_eq!(
            out.side_effect,
            SideEffect::DisplayMessage("hello zach".into())
        );
    }

    #[test]
    fn display_message_without_arg_is_error() {
        let mut reg = CommandRegistry::new();
        register_tier1(&mut reg);
        let cmd = parse("display-message").unwrap().remove(0);
        let mut ws = Workspace::spawn_single("/bin/sh", PtySize { rows: 24, cols: 80 }).unwrap();
        let mut ctx = CommandContext {
            workspace: &mut ws,
            caller_pane: 0,
            caller_client: ClientId(0),
            format: FormatContext {
                session_name: "".into(),
                session_id: "".into(),
                window_index: 0,
                window_name: "".into(),
                window_id: "".into(),
                pane_index: 0,
                pane_id: "".into(),
                pane_current_command: "".into(),
                host: "".into(),
                host_short: "".into(),
                user: "".into(),
            },
        };
        assert!(dispatch(&cmd, &reg, &mut ctx).is_err());
    }
}
