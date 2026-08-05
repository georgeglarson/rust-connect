//! Route-table lint
//!
//! Honest surfaces: an advertised control that 404s is a defect of the same
//! class as an advertised capability with no backend. This file holds three
//! invariants, each with a clear pass condition and a clear failure message:
//!
//! 1. Every path declared in the generated OpenAPI spec is wired in the
//!    router, and every router path appears in the OpenAPI spec.
//!    (`test_router_paths_match_openapi_paths`)
//!
//! 2. Every `/api/v1/...` URL referenced from the web UI is wired in the
//!    router.
//!    (`test_ui_endpoints_are_wired`)
//!
//! 3. (Sanity) The OpenAPI spec and live router agree on the set of paths,
//!    modulo the SSE channel (`/api/v1/events`) which is deliberately
//!    excluded from OpenAPI.
//!    (`test_router_paths_match_openapi_paths`)
//!
//! Why this is a lint, not a runtime test: the routes a build compiles in
//! are the surface a caller sees; if the source declares a route but the
//! router doesn't, the next caller 404s. Catching it at `cargo test` time is
//! the gate this whole task is built for — "the next unwired button fails
//! CI".

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use rust_connect::api::openapi::ApiDoc;
use utoipa::OpenApi;

/// Convert an axum path template (`/api/v1/devices/:device_id/...`) to its
/// OpenAPI/URI-template form (`/api/v1/devices/{device_id}/...`). A single
/// colon-prefixed segment is rewritten; consecutive colons are illegal in
/// axum so we never have to worry about `::`.
fn axum_to_openapi(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if let Some(stripped) = seg.strip_prefix(':') {
                format!("{{{stripped}}}")
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Extract every `.route("...")` path string from a router source file.
/// Handles both the one-line form `.route("/path", method(handler))` and the
/// multi-line form where the path appears on the line following `.route(`.
fn extract_router_paths(source: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut lines = source.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(".route(") {
            // One-line form: .route("path", method(...))
            if let Some(path) = quoted_first_string(rest) {
                paths.push(path.to_string());
                continue;
            }
            // Multi-line form: look for "path" on the next line.
            if let Some(next) = lines.peek() {
                let next_trimmed = next.trim_start();
                if let Some(path) = quoted_first_string(next_trimmed) {
                    paths.push(path.to_string());
                    lines.next();
                }
            }
        }
    }
    paths
}

/// Return the first `"..."`-quoted string slice in `s`, or None.
fn quoted_first_string(s: &str) -> Option<&str> {
    let start = s.find('"')?;
    let after = &s[start + 1..];
    let end_rel = after.find('"')?;
    Some(&after[..end_rel])
}

/// Locate the repo root from CARGO_MANIFEST_DIR (which is the crate root for
/// integration tests in this crate).
fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// True for paths that the lint treats as known divergences between the
/// router and the OpenAPI spec. Each entry here must have a written reason
/// in a comment above the function — adding a path to this list is the
/// equivalent of marking it `INTENTIONAL-DIVERGENCE` in the feature ledger.
fn is_excluded_path(path: &str) -> bool {
    // UI plumbing: mounted only when settings.ui_enabled; never carries
    // an OpenAPI annotation because it's not API surface.
    matches!(path, "/" | "/ui" | "/ui/" | "/ui/index.html")
        // SSE channel: utoipa can't model an event-stream body, and the
        // spec deliberately excludes it (see src/api/openapi.rs tests +
        // /api-docs/openapi.json at runtime).
        || path == "/api/v1/events"
}

/// Every route in `src/api/router.rs` must appear in the OpenAPI spec, and
/// every OpenAPI path (excluding the SSE channel) must appear in the router.
/// Path-template syntax is normalized (axum `:device_id` → OpenAPI
/// `{device_id}`) before comparison.
#[test]
fn test_router_paths_match_openapi_paths() {
    let router_source = fs::read_to_string(repo_path("src/api/router.rs"))
        .expect("src/api/router.rs must be readable");
    let router_paths: BTreeSet<String> = extract_router_paths(&router_source)
        .into_iter()
        .map(|p| axum_to_openapi(&p))
        // UI routes are not part of the API surface and never appear in
        // OpenAPI by design; skip them. The SSE channel
        // (`/api/v1/events`) is also deliberately excluded from OpenAPI
        // (utoipa can't model an event-stream body); skip it here so
        // the parity check doesn't false-positive on it.
        .filter(|p| !is_excluded_path(p))
        .collect();

    let openapi_spec = ApiDoc::openapi();
    let openapi_paths: BTreeSet<String> = openapi_spec.paths.paths.keys().cloned().collect();

    let sse = "/api/v1/events";
    let openapi_paths_minus_sse: BTreeSet<String> = openapi_paths
        .iter()
        .filter(|p| p.as_str() != sse)
        .cloned()
        .collect();

    let only_in_router: BTreeSet<&String> = router_paths
        .iter()
        .filter(|p| !openapi_paths_minus_sse.contains(p.as_str()))
        .collect();
    assert!(
        only_in_router.is_empty(),
        "paths declared in router.rs but missing from the OpenAPI spec:\n  {}\n\
         either add the handler to src/api/openapi.rs `paths(...)` or \
         remove the route.",
        only_in_router
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let only_in_openapi: BTreeSet<&String> = openapi_paths_minus_sse
        .iter()
        .filter(|p| !router_paths.contains(p.as_str()))
        .collect();
    assert!(
        only_in_openapi.is_empty(),
        "paths declared in the OpenAPI spec but missing from router.rs:\n  {}\n\
         either add the .route(\"...\", ...) entry to src/api/router.rs or \
         remove the OpenAPI annotation.",
        only_in_openapi
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Every `/api/v1/...` URL referenced from the web UI must be wired in the
/// router. The UI is built up by hand against these paths; an unwired path
/// means the button on the page silently 404s.
#[test]
fn test_ui_endpoints_are_wired() {
    let ui_source = fs::read_to_string(repo_path("src/api/ui/index.html"))
        .expect("src/api/ui/index.html must be readable");
    let router_source = fs::read_to_string(repo_path("src/api/router.rs"))
        .expect("src/api/router.rs must be readable");

    // Extract path templates: `/api/v1/foo/${currentDeviceId}/bar`. The
    // `${...}` interpolations are filled at runtime; we keep the bare
    // template here and substitute a sentinel segment so router comparison
    // is meaningful.
    let mut ui_paths: BTreeSet<String> = BTreeSet::new();
    let mut i = 0;
    let bytes = ui_source.as_bytes();
    while i + 8 <= bytes.len() {
        if &bytes[i..i + 8] == b"/api/v1/" {
            if let Some(end_rel) = ui_source[i..]
                .find(|c: char| c == '"' || c == '\'' || c == ')' || c == '`' || c.is_whitespace())
            {
                let raw = &ui_source[i..i + end_rel];
                let template = raw.replace("${currentDeviceId}", ":device_id");
                // Don't include paths that are obviously dynamic
                // fragments (e.g. `?key=val`) — those don't represent a
                // route.
                let cleaned = template.split('?').next().unwrap_or(&template).to_string();
                if cleaned.starts_with("/api/v1/") && cleaned.len() > 8 {
                    ui_paths.insert(cleaned);
                }
                i += end_rel;
                continue;
            }
        }
        i += 1;
    }

    let router_paths: BTreeSet<String> = extract_router_paths(&router_source).into_iter().collect();

    let unwired: Vec<&String> = ui_paths
        .iter()
        .filter(|p| !router_paths.contains(p.as_str()))
        .collect();

    assert!(
        unwired.is_empty(),
        "UI references these paths but the router has no .route(\"...\") for them:\n  {}\n\
         the matching button will 404 at runtime.",
        unwired
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}
