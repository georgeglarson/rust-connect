//! API middleware
//!
//! Single Responsibility: Request/response middleware (CORS, logging, request ID, rate limiting).

use axum::extract::{Request, State};
use axum::http::header::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;
use tracing::info;

pub async fn security_headers(request: Request, next: Next) -> Response {
    use axum::http::header;

    let mut response = next.run(request).await;

    let headers = [
        (
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        (header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
        (
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ),
    ];

    for (name, value) in headers {
        response.headers_mut().insert(name, value);
    }

    response.headers_mut().insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("geolocation=(), microphone=(), camera=()"),
    );

    response
}

#[derive(Clone)]
struct ClientRateLimiter {
    window_start: Instant,
    count: u64,
}

static RATE_LIMITERS: std::sync::OnceLock<moka::sync::Cache<IpAddr, ClientRateLimiter>> =
    std::sync::OnceLock::new();

/// Fires at most once if `ConnectInfo<SocketAddr>` is missing from a request,
/// which silently collapses every caller into a single LOCALHOST bucket and
/// turns the per-IP limiter into a global one. In production the extension is
/// installed by `into_make_service_with_connect_info`
/// (`services/service_manager.rs`), so a hit here means a wiring regression.
///
/// NOT a `debug_assert!`: the API integration tests drive `build_router` with
/// `oneshot`, which has no `ConnectInfo` by design, so an assert would abort
/// the test suite instead of catching anything.
static MISSING_CONNECT_INFO: std::sync::Once = std::sync::Once::new();

pub async fn rate_limiter(request: Request, next: Next) -> Response {
    let limiters = RATE_LIMITERS.get_or_init(|| {
        moka::sync::Cache::builder()
            .time_to_live(std::time::Duration::from_secs(60))
            .max_capacity(10000)
            .build()
    });

    let client_ip = match request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
    {
        Some(connect_info) => connect_info.0.ip(),
        None => {
            MISSING_CONNECT_INFO.call_once(|| {
                tracing::warn!(
                    event = "rate_limiter_missing_connect_info",
                    "No ConnectInfo on the request; every caller shares one rate-limit bucket"
                );
            });
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
    };

    let mut entry = limiters
        .get(&client_ip)
        .unwrap_or_else(|| ClientRateLimiter {
            window_start: Instant::now(),
            count: 0,
        });

    if entry.window_start.elapsed().as_secs() >= 60 {
        entry.window_start = Instant::now();
        entry.count = 0;
    }
    entry.count += 1;
    let is_rate_limited = entry.count > 100;

    limiters.insert(client_ip, entry);

    if is_rate_limited {
        let mut resp = Response::new(axum::body::Body::from("Rate limit exceeded"));
        *resp.status_mut() = axum::http::StatusCode::TOO_MANY_REQUESTS;
        resp.headers_mut()
            .insert("Retry-After", HeaderValue::from_static("60"));
        return resp;
    }

    next.run(request).await
}

pub async fn auth_middleware(
    State(state): State<Arc<crate::app::AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, impl IntoResponse> {
    // security-audit-2026-08-20 P3: query-string `api_key` is honored on every
    // route today. EventSource (the UI's `/api/v1/events` consumer) cannot
    // set request headers, so the SSE route must keep accepting the query
    // form. Everywhere else the header is mandatory — a leaked query key
    // (Referer, server access logs, shell history) would otherwise
    // authenticate pairing, sharing, clipboard, SMS, and lock endpoints.
    let allow_query = req.uri().path() == "/api/v1/events";

    let mut api_key = None;

    if let Some(key) = req.headers().get("X-API-Key").and_then(|h| h.to_str().ok()) {
        api_key = Some(key.to_string());
    } else if allow_query {
        if let Some(query) = req.uri().query() {
            for param in query.split('&') {
                let mut parts = param.splitn(2, '=');
                if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                    if k == "api_key" {
                        api_key = Some(v.to_string());
                        break;
                    }
                }
            }
        }
    }

    let is_valid = if let Some(key) = api_key {
        crate::api::auth::validate_api_key(&key, &state.settings.api_keys).is_ok()
    } else {
        false
    };

    if !is_valid {
        let error_response = (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "UNAUTHORIZED",
                    "message": "Invalid or missing API key"
                }
            })),
        );
        return Err(error_response);
    }

    Ok(next.run(req).await)
}

/// Path-only form of a request URI for logging. Query strings may carry
/// secrets (`?api_key=` for SSE) or user content (share filenames), so they
/// are never written to the log.
fn loggable_path(uri: &axum::http::Uri) -> &str {
    uri.path()
}

pub async fn request_logger(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = loggable_path(request.uri()).to_string();
    let request_id = uuid::Uuid::new_v4().to_string();

    info!(
        method = %method,
        path = %path,
        request_id = %request_id,
        event = "api_request",
        "Incoming API request"
    );

    let mut response = next.run(request).await;

    response.headers_mut().insert(
        "X-Request-ID",
        HeaderValue::from_str(&request_id).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );

    info!(
        status = response.status().as_u16(),
        request_id = %request_id,
        event = "api_response",
        "API response"
    );

    response
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_loggable_path_strips_query_string() {
        let uri: axum::http::Uri = "/api/v1/events?api_key=secret123".parse().unwrap();
        assert_eq!(loggable_path(&uri), "/api/v1/events");
    }

    #[test]
    fn test_loggable_path_without_query() {
        let uri: axum::http::Uri = "/api/v1/devices".parse().unwrap();
        assert_eq!(loggable_path(&uri), "/api/v1/devices");
    }

    /// The fallback path (no `ConnectInfo` extension) must still serve the
    /// request. It only collapses every caller into one bucket, which the
    /// warn-once records; it must never fail the request.
    #[tokio::test]
    async fn test_rate_limiter_serves_request_without_connect_info() {
        use axum::body::Body;
        use axum::http::{Request as HttpRequest, StatusCode};
        use tower::ServiceExt;

        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(rate_limiter));

        let response = app
            .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
