//! Schema-lint for `docs/functional-coverage.md`.
//!
//! Slice 0A keeps this thin: it parses the three matrix YAML blocks and
//! refuses to merge:
//!
//! - a non-allowed status value;
//! - a non-PASS row missing a `reason`;
//! - a missing row for any Rust plugin or upstream-only capability row
//!   from `tests/fixtures/{rust-capabilities,upstream-capabilities}/...`;
//! - a duplicate `feature` value within a matrix.
//!
//! Slice 0B can layer richer invariants on top without touching the
//! ledger format, since the file's machine-readable part stays fenced YAML.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const LEDGER_PATH: &str = "docs/functional-coverage.md";
const RUST_FIXTURE: &str = "tests/fixtures/rust-capabilities.yaml";
const UPSTREAM_FILES: &[&str] = &[
    "tests/fixtures/upstream-capabilities/kdeconnect-kde.yaml",
    "tests/fixtures/upstream-capabilities/gsconnect.yaml",
    "tests/fixtures/upstream-capabilities/kdeconnect-android.yaml",
];

const ALLOWED_STATUSES: &[&str] = &[
    "PASS",
    "FAIL",
    "UNVERIFIED",
    "NOT-APPLICABLE",
    "INTENTIONAL-DIVERGENCE",
];

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Row {
    feature: String,
    status: String,
    reason: String,
    upstream: String,
    rust_impl: Option<bool>,
}

fn read(path: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {}", p.display(), e))
}

/// Walk the ledger file and extract fenced YAML blocks. The matrix sections
/// start with ` ```yaml ` and end with the closing fence. They live under
/// section headings like `## Feature ledger` (label = "feature_ledger"),
/// `## Environment matrix` ("environment_matrix"), and `## Device matrix`
/// ("device_matrix"). Each block's content is appended verbatim.
fn extract_yaml_blocks(text: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, (String, String)> = BTreeMap::new(); // label -> (heading, body)
    let mut current_heading: Option<String> = None;
    let mut in_yaml = false;
    let mut current_block_label: Option<String> = None;
    let mut body = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        // Markdown section heading?
        if !in_yaml && trimmed.starts_with("## ") {
            current_heading = Some(
                trimmed
                    .trim_start_matches(|c: char| c == '#' || c == ' ' || c == '\t')
                    .to_string(),
            );
            continue;
        }
        // Fence open/close
        if trimmed.starts_with("```") {
            if !in_yaml {
                let rest = trimmed.trim_start_matches("```").trim();
                if rest == "yaml" {
                    // Open
                    in_yaml = true;
                    // Look up label from current heading
                    let heading = current_heading.clone().unwrap_or_default();
                    let label = heading_to_label(&heading);
                    current_block_label = Some(label);
                    body.clear();
                }
            } else {
                // close
                if let Some(label) = current_block_label.take() {
                    out.insert(
                        label,
                        (current_heading.clone().unwrap_or_default(), body.clone()),
                    );
                    body.clear();
                }
                in_yaml = false;
            }
            continue;
        }
        if in_yaml {
            body.push_str(line);
            body.push('\n');
        }
    }
    out.into_iter().map(|(k, (_, v))| (k, v)).collect()
}

fn heading_to_label(heading: &str) -> String {
    let lower = heading.to_ascii_lowercase();
    if lower.starts_with("feature ledger") {
        "feature_ledger".to_string()
    } else if lower.starts_with("environment matrix") {
        "environment_matrix".to_string()
    } else if lower.starts_with("device matrix") {
        "device_matrix".to_string()
    } else {
        format!("unknown#{}", heading)
    }
}

/// Minimal YAML parser for the ledger's restricted shape.
/// The matrices are sequences of mappings, where each mapping is exactly one
/// line per key starting with `  - feature: X` / `    key: value` / etc.
/// We accept either flow-style inline arrays (`[PASS, FAIL]`) or block style
/// (`  - PASS` on subsequent lines). For each row we read all top-level keys
/// up to the next `  - feature:` start.
fn parse_block_rows(body: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut iter = body.lines().peekable();
    let mut current: Option<BTreeMap<String, String>> = None;
    while let Some(line) = iter.next() {
        let indent = line.len() - line.trim_start().len();
        let body = line.trim_start();
        // Sequence marker starts a new row
        if indent == 2 && body.starts_with("- ") {
            if let Some(row) = current.take() {
                rows.push(row_to_struct(row));
            }
            let rest = body.trim_start_matches("- ").trim();
            // Could be either `feature: X` or a flow-style list start
            let mut entry: BTreeMap<String, String> = BTreeMap::new();
            if let Some((k, v)) = rest.split_once(':') {
                entry.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
            }
            current = Some(entry);
            continue;
        }
        // Mid-row key
        if indent >= 4 && !body.starts_with("- ") && current.is_some() {
            if let Some((k, v)) = body.split_once(':') {
                let k = k.trim();
                let v = v.trim();
                let row = current.as_mut().expect("current row");
                if v.is_empty() {
                    // header-only key; expect list on following lines
                    row.entry(k.to_string()).or_insert_with(String::new);
                } else {
                    // Flow style?
                    let cleaned = v.trim_matches('"').to_string();
                    row.insert(k.to_string(), cleaned);
                }
            }
            continue;
        }
        // List item under a header-only key (status list isn't used here; we
        // emit each row's status as a single scalar, but this allows extra
        // content under side-headings).
        if body.starts_with("- ") && current.is_some() {
            let item = body
                .trim_start_matches("- ")
                .trim()
                .trim_matches('"')
                .to_string();
            let row = current.as_mut().expect("current row");
            // Heuristic: if last header-key is empty, fill it
            if let Some((_, v)) = row.iter_mut().last() {
                if v.is_empty() {
                    *v = item;
                    continue;
                }
            }
            row.entry("items".to_string()).or_insert(item);
        }
    }
    if let Some(row) = current.take() {
        rows.push(row_to_struct(row));
    }
    rows
}

fn row_to_struct(map: BTreeMap<String, String>) -> Row {
    Row {
        feature: map.get("feature").cloned().unwrap_or_default(),
        status: map.get("status").cloned().unwrap_or_default(),
        reason: map.get("reason").cloned().unwrap_or_default(),
        upstream: map.get("upstream").cloned().unwrap_or_default(),
        rust_impl: map.get("rust_impl").and_then(|v| match v.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }),
    }
}

fn rust_plugin_names() -> BTreeSet<String> {
    let raw = read(RUST_FIXTURE);
    let mut names = BTreeSet::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("  - name:") {
            names.insert(rest.trim().to_string());
        }
    }
    names
}

/// Upstream role names from `tests/fixtures/upstream-capabilities/*.yaml`.
/// Each entry starts with `  - role: <name>`; we prefix with the
/// implementation to mirror the ledger's `feature` value (e.g.
/// `kdeconnect-kde/battery`).
fn upstream_roles() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let raw_kde = read(UPSTREAM_FILES[0]);
    for line in raw_kde.lines() {
        if let Some(rest) = line.strip_prefix("  - role:") {
            out.insert(format!("kdeconnect-kde/{}", rest.trim()));
        }
    }
    let raw_gsc = read(UPSTREAM_FILES[1]);
    for line in raw_gsc.lines() {
        if let Some(rest) = line.strip_prefix("  - role:") {
            out.insert(format!("gsconnect/{}", rest.trim()));
        }
    }
    let raw_and = read(UPSTREAM_FILES[2]);
    for line in raw_and.lines() {
        if let Some(rest) = line.strip_prefix("  - role:") {
            out.insert(format!("kdeconnect-android/{}", rest.trim()));
        }
    }
    out
}

#[test]
fn functional_coverage_ledger_is_consistent() {
    let ledger_text = read(LEDGER_PATH);
    let blocks = extract_yaml_blocks(&ledger_text);
    assert!(
        blocks.contains_key("feature_ledger"),
        "ledger is missing the `## Feature ledger` fenced YAML block"
    );
    assert!(
        blocks.contains_key("environment_matrix"),
        "ledger is missing the `## Environment matrix` fenced YAML block"
    );
    assert!(
        blocks.contains_key("device_matrix"),
        "ledger is missing the `## Device matrix` fenced YAML block"
    );

    let feature_rows = parse_block_rows(blocks.get("feature_ledger").unwrap());
    let env_rows = parse_block_rows(blocks.get("environment_matrix").unwrap());
    let device_rows = parse_block_rows(blocks.get("device_matrix").unwrap());

    let allowed: BTreeSet<&str> = ALLOWED_STATUSES.iter().copied().collect();

    // Per-row status / reason checks (per-matrix: same feature may legitimately
    // appear in feature_ledger AND environment_matrix AND device_matrix)
    for (label, rows) in [
        ("feature_ledger", &feature_rows),
        ("environment_matrix", &env_rows),
        ("device_matrix", &device_rows),
    ] {
        let mut seen_features: BTreeSet<String> = BTreeSet::new();
        for row in rows.iter() {
            assert!(!row.feature.is_empty(), "{} row missing `feature`", label);
            assert!(
                !seen_features.contains(&row.feature),
                "{} has duplicate feature row: `{}`",
                label,
                row.feature
            );
            seen_features.insert(row.feature.clone());

            assert!(
                allowed.contains(row.status.as_str()),
                "{} row `{}` has status `{}`; allowed: {:?}",
                label,
                row.feature,
                row.status,
                ALLOWED_STATUSES
            );
            if row.status != "PASS" {
                assert!(
                    !row.reason.trim().is_empty(),
                    "{} row `{}` is `{}` and must carry a `reason`",
                    label,
                    row.feature,
                    row.status
                );
            }
        }
    }

    // Coverage: every Rust plugin must have a feature row.
    let rust_set = rust_plugin_names();
    let feature_names: BTreeSet<String> = feature_rows.iter().map(|r| r.feature.clone()).collect();
    let missing_rust: Vec<String> = rust_set
        .iter()
        .filter(|n| !feature_names.contains(*n))
        .cloned()
        .collect();
    assert!(
        missing_rust.is_empty(),
        "feature ledger is missing rows for these rust plugins: {:?}",
        missing_rust
    );

    // Coverage: every upstream role name (as `<impl>/<role>`) must have a row.
    let upstreams_required = upstream_roles();
    let missing_up: Vec<String> = upstreams_required
        .iter()
        .filter(|n| !feature_names.contains(*n))
        .cloned()
        .collect();
    assert!(
        missing_up.is_empty(),
        "feature ledger is missing rows for these upstream roles: {:?}",
        missing_up
    );

    // Sanity: at least one feature row, at least one env row, at least one
    // device row. Catches a regression where the parser interprets a block
    // as empty.
    assert!(
        !feature_rows.is_empty(),
        "feature_ledger parsed to zero rows"
    );
    assert!(
        !env_rows.is_empty(),
        "environment_matrix parsed to zero rows"
    );
    assert!(!device_rows.is_empty(), "device_matrix parsed to zero rows");

    // The lint is compile-time anchored to these helpers; touching the body
    // keeps them used.
    let _ = rust_plugin_names();
    let _ = upstream_roles();
}
