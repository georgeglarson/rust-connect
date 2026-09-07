//! OpenAPI lint (2026-09-02 audit, A5).
//!
//! `docs/constitution.md`: "`/docs` is the contract... If it's not in the
//! spec, it doesn't exist." A `$ref` to a schema that was never registered
//! is worse than absent: Swagger UI and every codegen consumer hit an
//! unresolvable reference. The live spec carried 11 of them, including the
//! flagship `Device` type. Every reference must resolve.

use std::collections::BTreeSet;

use rust_connect::api::openapi::ApiDoc;
use rust_connect::api::types::UNTYPED_API_ALIASES;
use utoipa::OpenApi;

#[test]
fn test_every_schema_ref_in_the_spec_resolves() {
    let spec = ApiDoc::openapi();
    let json = serde_json::to_value(&spec).expect("spec serializes");

    let registered: BTreeSet<String> = json["components"]["schemas"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    let mut referenced = BTreeSet::new();
    collect_refs(&json, &mut referenced);

    let dangling: Vec<&String> = referenced
        .iter()
        .filter(|name| !registered.contains(*name))
        .collect();
    assert!(
        dangling.is_empty(),
        "OpenAPI `$ref`s with no registered schema: {dangling:?}"
    );
}

fn collect_refs(value: &serde_json::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(r)) = map.get("$ref") {
                if let Some(name) = r.strip_prefix("#/components/schemas/") {
                    out.insert(name.to_string());
                }
            }
            map.values().for_each(|v| collect_refs(v, out));
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_refs(v, out)),
        _ => {}
    }
}

/// 2026-09-06 audit B6: `GET /api/v1/devices` honoured `page` and `limit`
/// that the spec never mentioned. Both list endpoints document them.
#[test]
fn test_paginated_list_endpoints_document_page_and_limit() {
    let spec = ApiDoc::openapi();
    let json = serde_json::to_value(&spec).expect("spec serializes");
    for path in ["/api/v1/devices", "/api/v1/notifications"] {
        let params = json["paths"][path]["get"]["parameters"]
            .as_array()
            .unwrap_or_else(|| panic!("{path} GET must declare parameters"));
        let names: BTreeSet<&str> = params.iter().filter_map(|p| p["name"].as_str()).collect();
        for wanted in ["page", "limit"] {
            assert!(
                names.contains(wanted),
                "{path} GET must document the `{wanted}` query parameter; has {names:?}"
            );
        }
    }
}

/// 2026-09-06 audit item 6: every endpoint's 200 body must be a typed
/// schema, not `GenericResponse` / `PingResponse` / any other alias of
/// `ApiResponse<serde_json::Value>`. The 2026-09-02 lint only checks
/// that `$ref`s resolve, which `GenericResponse` does (it is registered)
/// — so the spec was passing every gate while telling codegen consumers
/// `data` was `{}`.
///
/// Pin: this number may only go DOWN. To type an endpoint, replace its
/// `body = GenericResponse` (or whichever untyped alias) with a typed
/// struct, drop the alias from [`UNTYPED_API_ALIASES`] if no longer
/// used, and lower the pin in the same commit.
#[test]
fn test_untyped_response_bodies_only_ever_decrease() {
    let spec = ApiDoc::openapi();
    let json = serde_json::to_value(&spec).expect("spec serializes");
    let untyped: BTreeSet<&str> = UNTYPED_API_ALIASES.iter().copied().collect();

    let mut count: usize = 0;
    let mut offenders: Vec<String> = Vec::new();
    if let Some(paths) = json["paths"].as_object() {
        for (path, path_item) in paths {
            for method in ["get", "post", "put", "delete", "patch"] {
                let Some(op) = path_item[method].as_object() else {
                    continue;
                };
                let Some(r200) = op["responses"]["200"].as_object() else {
                    continue;
                };
                let Some(content) = r200.get("content").and_then(|c| c.as_object()) else {
                    continue;
                };
                let Some(json) = content.get("application/json") else {
                    continue;
                };
                let Some(schema_ref) = json
                    .get("schema")
                    .and_then(|s| s.get("$ref"))
                    .and_then(|r| r.as_str())
                else {
                    continue;
                };
                let Some(name) = schema_ref.strip_prefix("#/components/schemas/") else {
                    continue;
                };
                if untyped.contains(name) {
                    count += 1;
                    offenders.push(format!("{method} {path} -> {name}"));
                }
            }
        }
    }

    // 2026-09-06 audit, base `main` (sha 91aed7c). Lower it in the same
    // commit that types an endpoint, never bump it. Battery endpoints
    // typed here: pin 35 → 33. Connectivity + telephony endpoints typed
    // here: pin 33 → 31. SFTP endpoints (request, info, mount, unmount)
    // typed here: pin 31 → 27. SMS endpoints (threads, thread, request,
    // send) typed here: pin 27 → 23. Note: `request_sms_threads` reused
    // the existing SentResponse, no new struct. MPRIS endpoints
    // (players, local-players, request, action) typed here: pin 23 → 19.
    // Note: `request_mpris` also reuses SentResponse.
    const PIN: usize = 19;
    assert!(
        count <= PIN,
        "untyped-response pin is {PIN}; this commit allows {count} (offenders: {offenders:#?}). \
         Did you add an endpoint that uses GenericResponse/PingResponse without lowering the pin?"
    );
}
