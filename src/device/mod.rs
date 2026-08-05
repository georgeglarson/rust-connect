//! Device management layer
//!
//! This module handles device lifecycle and state management with strict SRP:
//! - Registry: Device storage (CRUD + persistence)
//! - Lifecycle: State transition enforcement
//! - Events: Event broadcasting to subscribers

pub mod events;
pub mod lifecycle;
pub mod registry;
pub mod types;

pub use events::EventBroadcaster;
pub use lifecycle::LifecycleManager;
pub use registry::DeviceRegistry;
pub use types::{Device, DeviceEvent, DeviceId, DeviceState, DeviceType};
