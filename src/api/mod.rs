//! API Layer - REST interface plus an SSE event stream
//!
//! This module provides the HTTP API for external consumers.
//! Each submodule has a single responsibility:
//! - router: Route mapping only
//! - handlers: Request processing only
//! - sse: Server-Sent Events streaming only
//! - auth: Authentication only
//! - middleware: Request/response middleware only

pub mod auth;
pub mod extractors;
pub mod handlers;
pub mod middleware;
pub mod openapi;
pub mod router;
pub mod sse;
pub mod types;

pub use router::build_router;
