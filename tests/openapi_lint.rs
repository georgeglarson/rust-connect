//! OpenAPI lint (2026-09-02 audit, A5).
//!
//! `docs/constitution.md`: "`/docs` is the contract... If it's not in the
//! spec, it doesn't exist." A `$ref` to a schema that was never registered
//! is worse than absent: Swagger UI and every codegen consumer hit an
//! unresolvable reference. The live spec carried 11 of them, including the
//! flagship `Device` type. Every reference must resolve.

use std::collections::BTreeSet;

use rust_connect::api::openapi::ApiDoc;
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
/// `body = <untyped alias>` with a typed struct and lower the pin in the
/// same commit. Detection reads the spec: a component whose `data` has no
/// schema is untyped.
#[test]
fn test_untyped_response_bodies_only_ever_decrease() {
    let spec = ApiDoc::openapi();
    let json = serde_json::to_value(&spec).expect("spec serializes");
    // Untyped means: the 200 body resolves to a component whose `data`
    // property has no schema at all (utoipa renders `serde_json::Value`
    // as `{}`). Derived from the spec, not from a list of alias names,
    // so a new `ApiResponse<serde_json::Value>` alias cannot slip past.
    let schemas = json["components"]["schemas"]
        .as_object()
        .expect("spec has component schemas");
    // A schema object "says something" when it is a `$ref` to a component
    // that itself says something, or carries a type/shape keyword. `{}` is
    // what utoipa emits for `serde_json::Value`; a missing `data` means the
    // body is not the envelope at all. Refs are followed a few levels so a
    // `data: {"$ref": Value}` chain cannot hide an empty schema.
    fn schema_is_typed(
        schema: &serde_json::Value,
        schemas: &serde_json::Map<String, serde_json::Value>,
        depth: u8,
    ) -> bool {
        let Some(obj) = schema.as_object() else {
            return false;
        };
        if let Some(r) = obj.get("$ref").and_then(|r| r.as_str()) {
            let Some(name) = r.strip_prefix("#/components/schemas/") else {
                return false;
            };
            return depth < 4
                && schemas
                    .get(name)
                    .is_some_and(|target| schema_is_typed(target, schemas, depth + 1));
        }
        [
            "type",
            "properties",
            "items",
            "allOf",
            "oneOf",
            "anyOf",
            "enum",
        ]
        .iter()
        .any(|k| obj.contains_key(*k))
    }
    // Every body is the `{status, data, metadata}` envelope with a typed
    // `data`, except the public liveness probe, which is a flat typed
    // struct by design (docs/constitution.md § 1 names it as the probe a
    // supervisor reads without unwrapping).
    const FLAT_BODY_OK: &[&str] = &["/api/v1/health"];
    let body_is_typed = |path: &str, schema: &serde_json::Value| -> bool {
        let mut envelope = schema.clone();
        // Follow the body ref to its component to look at `data`.
        if let Some(r) = schema.get("$ref").and_then(|r| r.as_str()) {
            let Some(name) = r.strip_prefix("#/components/schemas/") else {
                return false;
            };
            let Some(component) = schemas.get(name) else {
                return false;
            };
            envelope = component.clone();
        }
        let data = &envelope["properties"]["data"];
        if data.is_null() && FLAT_BODY_OK.contains(&path) {
            return schema_is_typed(&envelope, schemas, 0);
        }
        schema_is_typed(data, schemas, 0)
    };

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
                // Inline schemas count too: a 200 body with no `$ref` and no
                // shape is as untyped as a bare `serde_json::Value` alias.
                let Some(schema) = json.get("schema") else {
                    count += 1;
                    offenders.push(format!("{method} {path} -> (no schema)"));
                    continue;
                };
                if !body_is_typed(path, schema) {
                    count += 1;
                    let name = schema
                        .get("$ref")
                        .and_then(|r| r.as_str())
                        .unwrap_or("(inline)");
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
    // Note: `request_mpris` also reuses SentResponse. Contacts endpoints
    // (sync, list) typed here: pin 19 → 17. Remote-command trigger typed
    // here: pin 17 → 16. Notifications endpoints (list, send, reply,
    // action, dismiss) typed here: pin 16 → 11. Note: get_notification_icon
    // serves image/png and is not in the ratchet.
    //
    // Remaining handlers typed in this commit (pin 11 → 0): share
    // (files list, send file, send text, send url), clipboard (get, set,
    // request), lock (device), findmyphone (ring), volume (set), system
    // volume (set local sink control), remotecontrol (pointer),
    // remotekeyboard (keypress), and the device lifecycle handlers
    // (ping, delete, connect, disconnect, get_state, list_connected).
    // `request_clipboard` reuses SentResponse; `set_clipboard` /
    // `send_ping` / `delete_device` / `connect_device` /
    // `disconnect_device` got their own types because the legacy
    // shape carried `removed`/`connected`/`disconnected` flags the
    // shared struct doesn't have.
    // The pin reached zero on 2026-09-06: no 200 body in the spec is
    // `serde_json::Value`. Equality, not `<=`, so it stays there.
    const PIN: usize = 0;
    assert_eq!(
        count, PIN,
        "the spec has {count} untyped 200 bodies (offenders: {offenders:#?}); every response \
         body must be a typed schema (docs/constitution.md § 1)"
    );
}
