//! Dead-knob lint (Task 1.7)
//!
//! Acceptance: "every config option and CLI flag either changes behavior
//! or is deleted." A documented setting nobody reads is a lie — the same
//! defect class as an advertised route that 404s (see
//! tests/route_table_lint.rs's own framing).
//!
//! Compile-time field enumeration isn't available in stable Rust without
//! a proc-macro, so this is the "disciplined test-with-list" the brief
//! allows: `AppSettings` derives `Serialize` with no `#[serde(skip)]` or
//! `#[serde(rename)]` anywhere on it (confirmed by
//! `test_settings_struct_has_no_skip_or_rename_attributes` below, which
//! is what keeps this technique honest), so serializing a `Default()`
//! instance yields a JSON object whose keys are EXACTLY the struct's
//! field names. A field added to `AppSettings` without a matching entry
//! in `KNOWN_FIELDS` fails `test_every_settings_field_is_registered`
//! immediately — that's the mechanism this whole lint depends on to
//! catch a new dead knob before it lands, not a promise that every listed
//! field is meaningfully used (only that a human looked and cited where).

use std::collections::BTreeSet;

use rust_connect::config::settings::AppSettings;

/// One entry per `AppSettings` field: the field name (must match the
/// struct exactly) and file:line evidence of a reader OUTSIDE
/// `config/settings.rs`. `bootstrap.rs`'s CLI-override assignments
/// (`settings.x = value`) don't count as readers on their own — every
/// entry below cites an actual read, or the hand-off site where the
/// field's whole behavior is carried into another component's
/// constructor.
const KNOWN_FIELDS: &[(&str, &str)] = &[
    (
        "device_name",
        "src/daemon.rs:89 (device identity), :95 (registry set_device_identity)",
    ),
    (
        "tcp_port",
        "src/services/service_manager.rs:38 (TcpListenerService::bind_port)",
    ),
    (
        "udp_port",
        "src/services/service_manager.rs:97 (DiscoveryService::new — bind + broadcast_addr, both legs)",
    ),
    ("cert_dir", "src/app.rs:44 (CertificateManager::new)"),
    (
        "data_dir",
        "src/app.rs:47 (devices.json path), src/daemon.rs:88",
    ),
    ("log_level", "src/bootstrap.rs:68 (init_logging_from_env)"),
    (
        "log_max_files",
        "src/bootstrap.rs:68 (init_logging_from_env)",
    ),
    (
        "api_enabled",
        "src/services/service_manager.rs:275 (gates whether the REST server starts at all)",
    ),
    (
        "api_port",
        "src/services/service_manager.rs:280 (REST server bind port)",
    ),
    (
        "api_bind",
        "src/services/service_manager.rs:281 (REST server bind address)",
    ),
    (
        "api_keys",
        "src/api/middleware.rs:136 (validate_api_key — request auth)",
    ),
    (
        "idle_timeout_secs",
        "src/protocol/listener.rs:251 (idle connection timeout)",
    ),
    (
        "ui_enabled",
        "src/api/router.rs:257 (mounts the /ui static handler)",
    ),
    (
        "allowed_origins",
        "src/api/router.rs:24 (CORS layer origin allowlist)",
    ),
];

#[test]
fn test_every_settings_field_is_registered() {
    let settings = AppSettings::default();
    let json = serde_json::to_value(&settings).expect("AppSettings must serialize");
    let actual_fields: BTreeSet<String> = json
        .as_object()
        .expect("AppSettings serializes to a JSON object")
        .keys()
        .cloned()
        .collect();

    let known_fields: BTreeSet<String> = KNOWN_FIELDS.iter().map(|(f, _)| f.to_string()).collect();

    let unregistered: Vec<&String> = actual_fields.difference(&known_fields).collect();
    assert!(
        unregistered.is_empty(),
        "AppSettings has field(s) with no registered reader evidence: {unregistered:?}\n\
         Either add a (field, reader-citation) entry to KNOWN_FIELDS in \
         tests/dead_knob_lint.rs — after confirming a real reader exists \
         outside config/settings.rs — or delete the field if nothing reads it."
    );

    let stale: Vec<&String> = known_fields.difference(&actual_fields).collect();
    assert!(
        stale.is_empty(),
        "KNOWN_FIELDS references field(s) no longer on AppSettings: {stale:?}\n\
         Remove the stale entry from tests/dead_knob_lint.rs."
    );
}

/// Sanity-checks the technique itself: if `AppSettings` ever grows a
/// `#[serde(skip)]` or `#[serde(rename)]` field, the serialize-and-diff
/// approach above would silently miss it (a skipped field disappears from
/// the JSON entirely; a renamed field would need `KNOWN_FIELDS` to use
/// the wire name instead of the Rust name). Pins the struct's current,
/// plain shape so a future attribute change surfaces here first, rather
/// than as a quietly-uncovered dead knob the other test can no longer see.
#[test]
fn test_settings_struct_has_no_skip_or_rename_attributes() {
    let source = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/config/settings.rs"),
    )
    .expect("src/config/settings.rs must be readable");
    let struct_block = source
        .split("pub struct AppSettings {")
        .nth(1)
        .and_then(|rest| rest.split("\n}").next())
        .expect("AppSettings struct body must be findable in src/config/settings.rs");

    assert!(
        !struct_block.contains("serde(skip") && !struct_block.contains("serde(rename"),
        "AppSettings gained a #[serde(skip...)] or #[serde(rename...)] field attribute — \
         the dead-knob lint's serialize-and-diff technique \
         (test_every_settings_field_is_registered) assumes struct field names equal \
         serialized JSON keys with no fields hidden. Either update that test's technique \
         to account for the new attribute, or avoid the attribute if a plain field will do."
    );
}
