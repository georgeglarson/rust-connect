//! Event broadcaster for device events
//!
//! Single Responsibility: Broadcast device events to subscribers.
//! Uses tokio broadcast channels for fan-out.

use std::fmt::Debug;

use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::device::types::DeviceEvent;

/// Generic event broadcaster using tokio broadcast channels.
/// Replaces the duplicated EventBroadcaster and PluginEventBroadcaster.
pub struct Broadcaster<T: Clone + Send + Sync + Debug> {
    tx: broadcast::Sender<T>,
    name: &'static str,
}

impl<T: Clone + Send + Sync + Debug> Broadcaster<T> {
    pub fn new(capacity: usize, name: &'static str) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx, name }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<T> {
        self.tx.subscribe()
    }

    pub fn broadcast(&self, event: T) {
        debug!(
            event_type = std::any::type_name_of_val(&event),
            "Broadcasting {} event", self.name
        );
        if self.tx.send(event).is_err() {
            warn!("No active subscribers to receive {} event", self.name);
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// Device event broadcaster (type alias for backward compatibility).
pub type EventBroadcaster = Broadcaster<DeviceEvent>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::device::types::{DeviceState, DeviceType};

    fn make_broadcaster() -> EventBroadcaster {
        EventBroadcaster::new(16, "device")
    }

    #[tokio::test]
    async fn test_broadcast_and_receive() {
        let broadcaster = make_broadcaster();
        let mut rx = broadcaster.subscribe();

        let event = DeviceEvent::Discovered {
            device_id: "test-device".to_string(),
            device_name: "Test".to_string(),
            device_type: DeviceType::Phone,
        };

        broadcaster.broadcast(event);
        let received = rx.recv().await.expect("Value expected to be present");

        match received {
            DeviceEvent::Discovered { device_id, .. } => {
                assert_eq!(device_id, "test-device");
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let broadcaster = make_broadcaster();
        let mut rx1 = broadcaster.subscribe();
        let mut rx2 = broadcaster.subscribe();

        assert_eq!(broadcaster.subscriber_count(), 2);

        let event = DeviceEvent::Removed {
            device_id: "dev".to_string(),
        };
        broadcaster.broadcast(event.clone());

        let r1 = rx1.recv().await.expect("Value expected to be present");
        let r2 = rx2.recv().await.expect("Value expected to be present");

        match (r1, r2) {
            (DeviceEvent::Removed { device_id: id1 }, DeviceEvent::Removed { device_id: id2 }) => {
                assert_eq!(id1, "dev");
                assert_eq!(id2, "dev");
            }
            _ => panic!("Wrong event types"),
        }
    }

    #[tokio::test]
    async fn test_no_subscribers_doesnt_panic() {
        let broadcaster = make_broadcaster();
        let event = DeviceEvent::Disconnected {
            device_id: "x".to_string(),
            reason: "test".to_string(),
        };
        broadcaster.broadcast(event);
    }

    #[tokio::test]
    async fn test_subscriber_receives_multiple_events() {
        let broadcaster = make_broadcaster();
        let mut rx = broadcaster.subscribe();

        for i in 0..5 {
            broadcaster.broadcast(DeviceEvent::StateChanged {
                device_id: format!("dev-{}", i),
                old_state: DeviceState::Discovered,
                new_state: DeviceState::Pairing,
            });
        }

        for i in 0..5 {
            match rx.recv().await.expect("Value expected to be present") {
                DeviceEvent::StateChanged { device_id, .. } => {
                    assert_eq!(device_id, format!("dev-{}", i));
                }
                _ => panic!("Wrong event type"),
            }
        }
    }

    #[tokio::test]
    async fn test_subscriber_count_after_drop() {
        let broadcaster = make_broadcaster();
        let rx1 = broadcaster.subscribe();
        assert_eq!(broadcaster.subscriber_count(), 1);
        drop(rx1);
        assert_eq!(broadcaster.subscriber_count(), 0);
    }
}
