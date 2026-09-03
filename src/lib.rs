#![warn(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

//! Rust Connect - A modern, API-first reimplementation of KDE Connect
//!
//! This library implements the KDE Connect protocol with:
//! - Single Responsibility Principle (SRP) throughout
//! - AI-first design (structured JSON everywhere)
//! - Clean separation of concerns
//!
//! # Architecture
//!
//! The library is organized into layers:
//!
//! - **Protocol Layer**: Discovery, connections, pairing, packet handling
//! - **Device Layer**: Device management, lifecycle, events
//! - **Plugin Layer**: Extensible plugin system for features
//! - **API Layer**: REST API and an SSE event stream for external consumers
//! - **Utils**: Error handling, logging, configuration
//!
//! # Examples
//!
//! ## Discovery
//!
//! ```no_run
//! use rust_connect::protocol::{DiscoveryService, Identity, types::DEFAULT_UDP_PORT};
//! use rust_connect::device::DeviceType;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let identity = Identity::new(
//!     Identity::generate_device_id(),
//!     "My Device".to_string(),
//!     DeviceType::Desktop,
//!     vec!["kdeconnect.ping".to_string()],
//!     vec!["kdeconnect.ping".to_string()],
//! );
//!
//! let service = DiscoveryService::new(identity, DEFAULT_UDP_PORT).await?;
//! service.broadcast().await?;
//! # Ok(())
//! # }
//! ```

// Public modules
pub mod api;
pub mod app;
pub mod bootstrap;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod device;
pub mod plugins;
pub mod protocol;
pub mod services;
pub mod utils;

// Re-export commonly used types for convenience
pub use device::{Device, DeviceEvent, DeviceId, DeviceState, DeviceType};
pub use protocol::{
    DiscoveryService, Identity, Packet, PacketSerializer, DEFAULT_TCP_PORT, DEFAULT_UDP_PORT,
    PROTOCOL_VERSION,
};
pub use utils::{init_logging, init_logging_from_env, Error, LogFormat, Result};

/// No-op. The rustls crypto provider is now selected per-config via
/// `builder_with_provider` (ring), so no process-global initialization is
/// needed. Kept for API compatibility with existing call sites.
pub fn init_crypto_provider() {}

/// The build's git sha, stamped by `build.rs` (vk #973); "unknown" outside
/// a git checkout.
pub const GIT_SHA: &str = env!("RC_GIT_SHA");

/// `<crate version> (<sha>[-dirty])` — what `--version` and the health
/// endpoint report, so the running binary can be compared to `origin/main`.
pub const BUILD_VERSION: &str = if matches!(env!("RC_GIT_DIRTY").as_bytes(), b"1") {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("RC_GIT_SHA"),
        "-dirty)"
    )
} else {
    concat!(env!("CARGO_PKG_VERSION"), " (", env!("RC_GIT_SHA"), ")")
};
