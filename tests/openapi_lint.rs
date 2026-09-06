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
