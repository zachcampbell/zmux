// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenMode {
    Primary,
    Alternate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum MouseTrackingMode {
    #[default]
    Off,
    Click,
    Drag,
    Motion,
}

impl MouseTrackingMode {
    pub fn captures_mouse(self) -> bool {
        self != Self::Off
    }

    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Click => 1,
            Self::Drag => 2,
            Self::Motion => 3,
        }
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Click),
            2 => Some(Self::Drag),
            3 => Some(Self::Motion),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelDirection {
    Up(usize),
    Down(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltScreenScrollPolicy {
    PaneScrollback,
    PassThrough,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelRouting {
    ScrollViewportUp(usize),
    ScrollViewportDown(usize),
    SendToApplication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseContext {
    pub screen_mode: ScreenMode,
    pub mouse_tracking_mode: MouseTrackingMode,
    pub alt_screen_policy: AltScreenScrollPolicy,
}

pub fn route_wheel(context: MouseContext, direction: WheelDirection) -> WheelRouting {
    match context.screen_mode {
        ScreenMode::Primary => map_direction(direction),
        ScreenMode::Alternate if context.mouse_tracking_mode.captures_mouse() => {
            WheelRouting::SendToApplication
        }
        ScreenMode::Alternate => match context.alt_screen_policy {
            AltScreenScrollPolicy::PaneScrollback => map_direction(direction),
            AltScreenScrollPolicy::PassThrough => WheelRouting::SendToApplication,
        },
    }
}

fn map_direction(direction: WheelDirection) -> WheelRouting {
    match direction {
        WheelDirection::Up(lines) => WheelRouting::ScrollViewportUp(lines),
        WheelDirection::Down(lines) => WheelRouting::ScrollViewportDown(lines),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AltScreenScrollPolicy, MouseContext, MouseTrackingMode, ScreenMode, WheelDirection,
        WheelRouting, route_wheel,
    };

    #[test]
    fn primary_screen_scrolls_the_viewport() {
        let routing = route_wheel(
            MouseContext {
                screen_mode: ScreenMode::Primary,
                mouse_tracking_mode: MouseTrackingMode::Off,
                alt_screen_policy: AltScreenScrollPolicy::PassThrough,
            },
            WheelDirection::Up(3),
        );

        assert_eq!(routing, WheelRouting::ScrollViewportUp(3));
    }

    #[test]
    fn alternate_screen_can_pass_through_when_app_owns_mouse() {
        let routing = route_wheel(
            MouseContext {
                screen_mode: ScreenMode::Alternate,
                mouse_tracking_mode: MouseTrackingMode::Click,
                alt_screen_policy: AltScreenScrollPolicy::PaneScrollback,
            },
            WheelDirection::Down(2),
        );

        assert_eq!(routing, WheelRouting::SendToApplication);
    }

    #[test]
    fn alternate_screen_policy_can_still_prefer_pane_scrollback() {
        let routing = route_wheel(
            MouseContext {
                screen_mode: ScreenMode::Alternate,
                mouse_tracking_mode: MouseTrackingMode::Off,
                alt_screen_policy: AltScreenScrollPolicy::PaneScrollback,
            },
            WheelDirection::Down(4),
        );

        assert_eq!(routing, WheelRouting::ScrollViewportDown(4));
    }
}
