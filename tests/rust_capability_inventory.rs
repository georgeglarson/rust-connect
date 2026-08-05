//! Tripwire: registered Rust plugins and their advertised capabilities must
//! match the committed fixture at `tests/fixtures/rust-capabilities.yaml`.
//!
//! The loader is the source of truth; the fixture is the tripwire. Any change
//! to a plugin's `name()`, `incoming_capabilities()`, or
//! `outgoing_capabilities()` without a matching fixture update must fail.
//!
//! Setting `UPDATE_RUST_CAPABILITIES_FIXTURE=1` rewrites the fixture from the
//! live loader (intended for the dev who intentionally changed registrations).
//! Without the env var this test only reads the fixture and compares.

use std::collections::BTreeSet;
use std::path::PathBuf;

use rust_connect::app::AppState;
use rust_connect::config::settings::AppSettings;
use rust_connect::plugins::{PluginAccess, PluginRegistry};

const FIXTURE_PATH: &str = "tests/fixtures/rust-capabilities.yaml";

#[derive(Debug, serde::Serialize)]
struct FixtureEntry {
    name: String,
    incoming_capabilities: Vec<String>,
    outgoing_capabilities: Vec<String>,
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_PATH)
}

fn parse_simple_yaml(text: &str) -> std::collections::BTreeMap<String, FixtureEntry> {
    // Hand-rolled parser for the limited YAML shape used by write_fixture:
    //
    //   plugins:
    //     - name: <plugin>
    //       incoming_capabilities:
    //         - <cap>
    //         - <cap>          # may also be `[]` for the empty literal
    //       outgoing_capabilities:
    //         - <cap>
    //
    // Indents are 2-space multiples. Anything else (real-world YAML) is unsupported.
    let mut out = std::collections::BTreeMap::new();
    #[derive(Debug, Clone, Copy)]
    enum Sec {
        None,
        Incoming,
        Outgoing,
    }
    let mut current: Option<FixtureEntry> = None;
    let mut section = Sec::None;

    for raw in text.lines() {
        // Strip trailing whitespace but preserve indent
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue; // blank / comment lines are not load-bearing for our shape
        }

        let indent = line.len() - line.trim_start().len();
        let body = line.trim_start();

        if indent == 0 && body == "plugins:" {
            continue; // header
        }

        if body.starts_with("- ") {
            // Could be either "- name: X" (entry opener, indent 2) or a list item (indent >= 4)
            let rest = body.trim_start_matches("- ").trim_start();
            if rest.starts_with("name:") {
                // Flush previous entry
                if let Some(entry) = current.take() {
                    out.insert(entry.name.clone(), entry);
                }
                let name = rest
                    .trim_start_matches("name:")
                    .trim()
                    .trim_matches('"')
                    .to_string();
                current = Some(FixtureEntry {
                    name,
                    incoming_capabilities: Vec::new(),
                    outgoing_capabilities: Vec::new(),
                });
                section = Sec::None;
                continue;
            }
            // Otherwise it's a capability list item
            if current.is_some() {
                let entry = current.as_mut().expect("entry open");
                match section {
                    Sec::Incoming => entry
                        .incoming_capabilities
                        .push(rest.trim_matches('"').to_string()),
                    Sec::Outgoing => entry
                        .outgoing_capabilities
                        .push(rest.trim_matches('"').to_string()),
                    Sec::None => {}
                }
            }
            continue;
        }

        if let Some((k, v)) = body.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            match k {
                "incoming_capabilities" => {
                    section = Sec::Incoming;
                    if !v.is_empty() && v != "[]" {
                        // inline single value (shouldn't happen with our writer)
                        if let Some(entry) = current.as_mut() {
                            entry
                                .incoming_capabilities
                                .push(v.trim_matches('"').to_string());
                        }
                        section = Sec::None;
                    }
                }
                "outgoing_capabilities" => {
                    section = Sec::Outgoing;
                    if !v.is_empty() && v != "[]" {
                        if let Some(entry) = current.as_mut() {
                            entry
                                .outgoing_capabilities
                                .push(v.trim_matches('"').to_string());
                        }
                        section = Sec::None;
                    }
                }
                _ => {
                    section = Sec::None;
                }
            }
            continue;
        }

        // Unknown line — clear section so we don't accidentally attribute the next list item
        section = Sec::None;
    }

    if let Some(entry) = current.take() {
        out.insert(entry.name.clone(), entry);
    }
    out
}

fn write_fixture(entries: &[FixtureEntry]) {
    let mut text = String::new();
    text.push_str("# Rust Connect plugin inventory\n");
    text.push_str("#\n");
    text.push_str("# Generated from the live PluginAccess::all() via the loader.\n");
    text.push_str("# Each entry: one block per plugin with name + both capability directions.\n");
    text.push_str("# Lifecycle-only plugins (no packet types) are recorded with empty lists.\n");
    text.push_str(
        "# The tripwire test refuses to pass when this fixture and the loader diverge.\n",
    );
    text.push_str(
        "# Regenerate after an intentional change with UPDATE_RUST_CAPABILITIES_FIXTURE=1.\n",
    );
    text.push_str("plugins:\n");
    for entry in entries {
        text.push_str(&format!("  - name: {}\n", entry.name));
        text.push_str("    incoming_capabilities:\n");
        if entry.incoming_capabilities.is_empty() {
            text.push_str("      []\n");
        } else {
            for c in &entry.incoming_capabilities {
                text.push_str(&format!("      - {}\n", c));
            }
        }
        text.push_str("    outgoing_capabilities:\n");
        if entry.outgoing_capabilities.is_empty() {
            text.push_str("      []\n");
        } else {
            for c in &entry.outgoing_capabilities {
                text.push_str(&format!("      - {}\n", c));
            }
        }
    }
    std::fs::write(fixture_path(), text).expect("write fixture");
}

async fn collect_live() -> Vec<FixtureEntry> {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let settings = AppSettings::default().with_data_dir(temp_dir.path().to_path_buf());
    let state = AppState::new_without_input(settings).expect("appstate");
    state.init_plugins().await;
    let infos = state.plugin_registry.list_with_capabilities().await;
    let plugins: &PluginAccess = &state.plugins;
    // Belt and braces: cross-check the loader's PluginAccess::all() agrees with
    // what the registry sees after init_plugins.
    let access_names: BTreeSet<String> =
        plugins.all().iter().map(|p| p.name().to_string()).collect();
    let registry_names: BTreeSet<String> = infos.iter().map(|i| i.name.clone()).collect();
    assert_eq!(
        access_names, registry_names,
        "PluginAccess::all() and PluginRegistry::list() disagreed; the loader is the source of truth, so a divergence here means a registration is broken (was `init_plugins` skipped?)"
    );
    // Silence unused-import warnings if `plugins` happens to be the only use of PluginRegistry
    let _ = std::any::type_name::<PluginRegistry>();
    let mut entries: Vec<FixtureEntry> = infos
        .into_iter()
        .map(|i| FixtureEntry {
            name: i.name,
            incoming_capabilities: i.incoming_capabilities,
            outgoing_capabilities: i.outgoing_capabilities,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

#[tokio::test]
async fn rust_capability_inventory_matches_fixture() {
    let entries = collect_live().await;

    if std::env::var("UPDATE_RUST_CAPABILITIES_FIXTURE").is_ok() {
        write_fixture(&entries);
        eprintln!(
            "rust-capabilities.yaml rewritten with {} plugins (set the env var to a known value to disable the tripwire test)"
            , entries.len()
        );
        return;
    }

    let raw = std::fs::read_to_string(fixture_path()).expect("read fixture");
    let mut fixture_entries: Vec<FixtureEntry> = parse_simple_yaml(&raw)
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    fixture_entries.sort_by(|a, b| a.name.cmp(&b.name));

    assert_eq!(
        entries.len(),
        fixture_entries.len(),
        "plugin count mismatch: live loader has {} plugins, fixture has {} (UPDATE_RUST_CAPABILITIES_FIXTURE=1 to regenerate)",
        entries.len(),
        fixture_entries.len()
    );

    for live in &entries {
        let fixture = fixture_entries
            .iter()
            .find(|f| f.name == live.name)
            .unwrap_or_else(|| {
                panic!(
                    "plugin `{}` is registered by the loader but absent from the fixture",
                    live.name
                )
            });
        let live_in: BTreeSet<&str> = live
            .incoming_capabilities
            .iter()
            .map(|s| s.as_str())
            .collect();
        let fix_in: BTreeSet<&str> = fixture
            .incoming_capabilities
            .iter()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            live_in, fix_in,
            "incoming_capabilities mismatch for plugin `{}`:\n  live: {:?}\n  fix:  {:?}\nUpdate fixture with UPDATE_RUST_CAPABILITIES_FIXTURE=1 if intended.",
            live.name,
            live.incoming_capabilities,
            fixture.incoming_capabilities
        );
        let live_out: BTreeSet<&str> = live
            .outgoing_capabilities
            .iter()
            .map(|s| s.as_str())
            .collect();
        let fix_out: BTreeSet<&str> = fixture
            .outgoing_capabilities
            .iter()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            live_out, fix_out,
            "outgoing_capabilities mismatch for plugin `{}`:\n  live: {:?}\n  fix:  {:?}\nUpdate fixture with UPDATE_RUST_CAPABILITIES_FIXTURE=1 if intended.",
            live.name,
            live.outgoing_capabilities,
            fixture.outgoing_capabilities
        );
    }

    for fixture in &fixture_entries {
        assert!(
            entries.iter().any(|l| l.name == fixture.name),
            "plugin `{}` is in the fixture but NOT registered by the loader; remove it from the fixture if intentional",
            fixture.name
        );
    }
}
