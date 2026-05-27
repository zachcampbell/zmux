// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Broadcast event bus for pane lifecycle and state changes.
//!
//! `Workspace` owns the bus; subscribers (supervisor overlay, MCP
//! server) attach and react without polling. Dropped subscribers are
//! pruned lazily on the next publish.
//!
//! Pane id is `u32` on the wire even though `PaneId` is `usize`
//! in-memory; JSON consumers want concrete sizes, so the cast
//! happens at the publish boundary.

use std::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PaneStateChanged {
        pane_id: u32,
        from: String,
        to: String,
    },
    PaneOutput {
        pane_id: u32,
        bytes_delta: u64,
        last_line_preview: String,
    },
    PaneExited {
        pane_id: u32,
        exit_code: i32,
    },
    PaneSpawned {
        pane_id: u32,
        label: Option<String>,
    },
    PaneClosed {
        pane_id: u32,
    },
    LabelChanged {
        pane_id: u32,
        label: Option<String>,
    },
}

/// Multi-producer, multi-consumer broadcast bus. Each subscriber owns a
/// `mpsc::Receiver<Event>`; `publish` clones the event into every live
/// sender and prunes any whose receiver has been dropped.
#[derive(Debug, Default)]
pub struct EventBus {
    subscribers: Vec<mpsc::Sender<Event>>,
}

impl EventBus {
    pub fn subscribe(&mut self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        self.subscribers.push(tx);
        rx
    }

    pub fn publish(&mut self, event: Event) {
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }

    /// Number of currently-attached subscribers. Test/diagnostic helper —
    /// production code should never branch on subscriber count.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_then_publish_delivers_event() {
        let mut bus = EventBus::default();
        let rx = bus.subscribe();
        bus.publish(Event::PaneClosed { pane_id: 7 });
        assert_eq!(rx.recv().unwrap(), Event::PaneClosed { pane_id: 7 });
    }

    #[test]
    fn dropped_subscriber_is_pruned_on_next_publish() {
        let mut bus = EventBus::default();
        {
            let _rx = bus.subscribe();
            bus.publish(Event::PaneClosed { pane_id: 1 });
        }
        bus.publish(Event::PaneClosed { pane_id: 2 });
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn multiple_subscribers_each_receive_a_copy() {
        let mut bus = EventBus::default();
        let rx1 = bus.subscribe();
        let rx2 = bus.subscribe();
        bus.publish(Event::PaneSpawned {
            pane_id: 3,
            label: Some("agent".to_string()),
        });
        assert_eq!(
            rx1.recv().unwrap(),
            Event::PaneSpawned {
                pane_id: 3,
                label: Some("agent".to_string()),
            }
        );
        assert_eq!(
            rx2.recv().unwrap(),
            Event::PaneSpawned {
                pane_id: 3,
                label: Some("agent".to_string()),
            }
        );
    }
}
