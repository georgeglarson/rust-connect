//! UI handlers
//!
//! Single Responsibility: Serve the web dashboard UI.

use axum::http::header;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};

const UI_HTML: &str = include_str!("../ui/index.html");

pub async fn serve_ui() -> impl IntoResponse {
    Html(UI_HTML)
}

pub async fn redirect_to_ui() -> impl IntoResponse {
    (StatusCode::FOUND, [(header::LOCATION, "/ui/")])
}
