//! Utility modules for Rust Connect

pub mod errors;
pub mod logging;

// Re-export commonly used types
pub use errors::{Error, Result};
pub use logging::{init_logging, init_logging_from_env, LogFormat};
