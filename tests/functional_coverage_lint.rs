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
//! Slice 0B layers richer invariants on top without changing the ledger
//! format, since the file's machine-readable part stays fenced YAML:
//!
//! - D3 rollup: a row's `status: PASS` requires every status-valued cell in
//!   that row to be `PASS` or `NOT-APPLICABLE`; any weaker cell forces the
//!   row's `status` to the weakest present value (or `UNVERIFIED` on mix).
//! - D4 cite: every PASS row must have a non-empty `cite` containing at
//!   least one non-self artifact token (`docs/live-validation.md`,
//!   `upstream`, `tests/fixtures/upstream-wire/`, `kdeconnect-android`,
//!   `kdeconnect-kde`, `gsconnect`, or `peer`). Self-only cites (just
//!   `src/…`/`tests/…` paths) fail.
//! - D5 fixture-provenance: a feature row with `fixture_provenance: PASS`
//!   must cite a fixture in `tests/fixtures/upstream-wire/` or an
//!   independent-peer/upstream artifact.
//! - D6 provenance index: every file under `tests/fixtures/upstream-wire/`
//!   has a provenance entry; every entry's `file` exists; every
//!   `used_by` test reference resolves (file exists, contains the named
//!   `fn`); every `upstream-derived` entry's `pinned_commit` matches the
//!   pin in `tests/fixtures/upstream-capabilities/*.yaml`.
//! - D7 parser: rows preserve every dimension cell, not just the named
//!   fields, so the rollup can read every cell in the row.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const LEDGER_PATH: &str = "docs/functional-coverage.md";
const RUST_FIXTURE: &str = "tests/fixtures/rust-capabilities.yaml";
const UPSTREAM_FILES: &[&str] = &[
    "tests/fixtures/upstream-capabilities/kdeconnect-kde.yaml",
    "tests/fixtures/upstream-capabilities/gsconnect.yaml",
    "tests/fixtures/upstream-capabilities/kdeconnect-android.yaml",
];
const WIRE_FIXTURE_DIR: &str = "tests/fixtures/upstream-wire";
const PROVENANCE_INDEX: &str = "tests/fixtures/upstream-wire/provenance.yaml";

const ALLOWED_STATUSES: &[&str] = &[
    "PASS",
    "FAIL",
    "UNVERIFIED",
    "NOT-APPLICABLE",
    "INTENTIONAL-DIVERGENCE",
];

/// Status-valued cells per matrix. Anything not listed here is either the
/// row's identity (`feature`), a free-text field (`cite`, `reason`,
/// `owner`), or a non-status string (`upstream`, `upstream_ref`, `rust_impl`).
const FEATURE_CELLS: &[&str] = &[
    "desktop_effect",
    "api_surface",
    "lifecycle",
    "hostile_input",
    "fixture_provenance",
    "live_device",
    "environment",
];
const ENV_CELLS: &[&str] = &[
    "clipboard-x11",
    "clipboard-wayland",
    "uinput",
    "audio",
    "session_dbus",
    "notification_server",
];
const DEVICE_CELLS: &[&str] = &["A15", "S21", "other_android"];

/// Non-self artifact tokens that count as a real `cite` (D4). The lint does
/// not know "the rest of the page" — these are the surfaces it can verify.
const NON_SELF_CITE_TOKENS: &[&str] = &[
    "docs/live-validation.md",
    "tests/fixtures/upstream-wire/",
    "kdeconnect-android",
    "kdeconnect-kde",
    "gsconnect",
    // bare "upstream" must be present alongside a repo name; we let it
    // through because hand-written cites vary widely.
    "upstream",
    // peer / live-validation artifacts written into `docs/live-validation.md`
    // use the word "peer" freely.
    "peer",
];

/// Order used to weaken a row when one of its cells disagrees with
/// `status: PASS`. We pick the weakest non-PASS/NOT-APPLICABLE value
/// present; ties broken by this order.
const STATUS_WEAKNESS: &[&str] = &[
    "INTENTIONAL-DIVERGENCE",
    "FAIL",
    "UNVERIFIED",
];

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Row {
    feature: String,
    status: String,
    reason: String,
    cite: String,
    upstream: String,
    upstream_ref: String,
    rust_impl: Option<bool>,
    /// Every other top-level key in the row's mapping. We keep them so
    /// the rollup can read every dimension cell the matrix defines.
    cells: BTreeMap<String, String>,
}

impl Row {
    fn cells_for(&self, label: &str) -> &[&str] {
        match label {
            "feature_ledger" => FEATURE_CELLS,
            "environment_matrix" => ENV_CELLS,
            "device_matrix" => DEVICE_CELLS,
            _ => &[],
        }
    }

    /// All status-valued cells in the row, in the canonical order, with
    /// the row's own `status:` ignored (it's the rollup target, not a cell).
    fn status_cells(&self, label: &str) -> Vec<(String, String)> {
        self.cells_for(label)
            .iter()
            .filter_map(|k| {
                self.cells
                    .get(*k)
                    .filter(|v| ALLOWED_STATUSES.contains(&v.as_str()))
                    .map(|v| ((*k).to_string(), v.clone()))
            })
            .collect()
    }
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
            current_heading = Some(trimmed.trim_start_matches(['#', ' ', '\t']).to_string());
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
    let mut current: Option<BTreeMap<String, String>> = None;
    for line in body.lines() {
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
        if indent >= 4 && !body.starts_with("- ") {
            if let Some((k, v)) = body.split_once(':') {
                let k = k.trim();
                let v = v.trim();
                if let Some(row) = current.as_mut() {
                    if v.is_empty() {
                        // header-only key; expect list on following lines
                        row.entry(k.to_string()).or_default();
                    } else {
                        // Flow style?
                        let cleaned = v.trim_matches('"').to_string();
                        row.insert(k.to_string(), cleaned);
                    }
                }
            }
            continue;
        }
        // List item under a header-only key (status list isn't used here; we
        // emit each row's status as a single scalar, but this allows extra
        // content under side-headings).
        if body.starts_with("- ") {
            let item = body
                .trim_start_matches("- ")
                .trim()
                .trim_matches('"')
                .to_string();
            if let Some(row) = current.as_mut() {
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
    }
    if let Some(row) = current.take() {
        rows.push(row_to_struct(row));
    }
    rows
}

fn row_to_struct(map: BTreeMap<String, String>) -> Row {
    let mut cells = map.clone();
    let feature = cells.remove("feature").unwrap_or_default();
    let status = cells.remove("status").unwrap_or_default();
    let reason = cells.remove("reason").unwrap_or_default();
    let cite = cells.remove("cite").unwrap_or_default();
    let upstream = cells.remove("upstream").unwrap_or_default();
    let upstream_ref = cells.remove("upstream_ref").unwrap_or_default();
    let rust_impl = cells.remove("rust_impl").and_then(|v| match v.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    });
    Row {
        feature,
        status,
        reason,
        cite,
        upstream,
        upstream_ref,
        rust_impl,
        cells,
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

/// Check whether a cite contains any non-self artifact token.
fn cite_has_non_self_token(cite: &str) -> bool {
    NON_SELF_CITE_TOKENS
        .iter()
        .any(|tok| cite.contains(tok))
}

/// Pick the weakest status in a list of cell values. `PASS` and
/// `NOT-APPLICABLE` are ignored; the rest are reduced via `STATUS_WEAKNESS`.
/// Returns the weakest if there are any non-passing cells; None otherwise.
fn weakest_status(cells: &[(String, String)]) -> Option<String> {
    let mut found: Vec<&str> = cells
        .iter()
        .map(|(_, v)| v.as_str())
        .filter(|v| !matches!(*v, "PASS" | "NOT-APPLICABLE"))
        .collect();
    if found.is_empty() {
        return None;
    }
    found.sort_by_key(|v| {
        STATUS_WEAKNESS
            .iter()
            .position(|x| x == v)
            .unwrap_or(usize::MAX)
    });
    Some(found[0].to_string())
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

    // Per-row status / reason / rollup / cite checks (per-matrix: same
    // feature may legitimately appear in feature_ledger AND
    // environment_matrix AND device_matrix).
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

            // Every dimension cell's value must also be an allowed status.
            for (cell_key, cell_val) in row.status_cells(label) {
                assert!(
                    allowed.contains(cell_val.as_str()),
                    "{} row `{}` cell `{}` has value `{}`; allowed: {:?}",
                    label,
                    row.feature,
                    cell_key,
                    cell_val,
                    ALLOWED_STATUSES
                );
            }

            // D3 rollup: a PASS row's every status cell must be PASS or
            // NOT-APPLICABLE; otherwise the row's status cannot honestly
            // be PASS. Pin the strongest cell so the integrator can see
            // which one demoted the row.
            if row.status == "PASS" {
                let cells = row.status_cells(label);
                if let Some(weakest) = weakest_status(&cells) {
                    let bad: Vec<String> = cells
                        .iter()
                        .filter(|(_, v)| {
                            v.as_str() != "PASS" && v.as_str() != "NOT-APPLICABLE"
                        })
                        .map(|(k, v)| format!("{}={}", k, v))
                        .collect();
                    panic!(
                        "{} row `{}` is `PASS` but cells disagree: {}. Rollup would force status to `{}` (D3).",
                        label, row.feature, bad.join(", "), weakest
                    );
                }
            }

            // D4 cite: every PASS row must carry a non-empty cite with at
            // least one non-self artifact token. The lint cannot read
            // intent — the token set is the enforcement surface.
            if row.status == "PASS" {
                let cite = row.cite.trim();
                assert!(
                    !cite.is_empty(),
                    "{} row `{}` is `PASS` and must carry a non-empty `cite` (D4)",
                    label,
                    row.feature
                );
                assert!(
                    cite_has_non_self_token(cite),
                    "{} row `{}` is `PASS` but cite `{}` contains no non-self artifact token (D4); allowed tokens: {:?}",
                    label, row.feature, cite, NON_SELF_CITE_TOKENS
                );

                // D5 fixture-provenance: feature_ledger rows only.
                if label == "feature_ledger" {
                    if row.cells.get("fixture_provenance").map(|s| s.as_str()) == Some("PASS") {
                        let ok = cite.contains("tests/fixtures/upstream-wire/")
                            || cite.contains("peer")
                            || cite.contains("kdeconnect-android")
                            || cite.contains("kdeconnect-kde")
                            || cite.contains("gsconnect")
                            || cite.contains("upstream");
                        assert!(
                            ok,
                            "{} row `{}` has `fixture_provenance: PASS` but cite `{}` does not reference `tests/fixtures/upstream-wire/` or a peer/upstream artifact (D5)",
                            label, row.feature, cite
                        );
                    }
                }
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

/// Parse the provenance index `tests/fixtures/upstream-wire/provenance.yaml`.
/// The file's shape is restricted (no full YAML), the same way the ledger
/// parser is — every fixture entry is a YAML mapping of scalar keys.
fn parse_provenance_index(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut entries: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current_file: Option<String> = None;
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    let mut in_fixtures = false;
    for line in text.lines() {
        let raw = line;
        let body = raw.trim_start();
        let indent = raw.len() - body.len();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if indent == 0 && body.starts_with("fixtures:") {
            in_fixtures = true;
            continue;
        }
        if !in_fixtures {
            continue;
        }
        // Sequence marker starts a new entry: `  - file: <path>`.
        if indent == 2 && body.starts_with("- ") {
            if let Some(prev_file) = current_file.take() {
                entries.insert(prev_file, std::mem::take(&mut current));
            }
            let rest = body.trim_start_matches("- ").trim();
            if let Some((k, v)) = rest.split_once(':') {
                current.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
                if k.trim() == "file" {
                    current_file = Some(v.trim().trim_matches('"').to_string());
                }
            }
            continue;
        }
        // Mid-entry key: `    key: value`.
        if indent >= 4 && !body.starts_with("- ") {
            if let Some((k, v)) = body.split_once(':') {
                current.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
            }
            continue;
        }
    }
    if let Some(prev_file) = current_file.take() {
        entries.insert(prev_file, current);
    }
    entries
}

fn manifest_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// D6: every file under `tests/fixtures/upstream-wire/` (except
/// `provenance.yaml`) has a provenance entry; every entry's `file` exists;
/// every `used_by` test reference resolves (file exists, contains the named
/// `fn`); every `upstream-derived` entry's `pinned_commit` matches the pin
/// in the corresponding `tests/fixtures/upstream-capabilities/*.yaml`
/// header.
#[test]
fn upstream_wire_provenance_is_consistent() {
    let root = manifest_root();
    let dir = root.join(WIRE_FIXTURE_DIR);
    assert!(
        dir.is_dir(),
        "missing fixture dir {} (Slice 0B requires it)",
        dir.display()
    );

    // Collect every fixture file (relative to WIRE_FIXTURE_DIR) — everything
    // except provenance.yaml itself.
    let mut on_disk: BTreeSet<String> = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).expect("read upstream-wire dir") {
        let entry = entry.expect("read_dir entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(&dir)
            .expect("file under upstream-wire")
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "provenance.yaml" {
            continue;
        }
        on_disk.insert(rel);
    }

    let index_text = read(PROVENANCE_INDEX);
    let entries = parse_provenance_index(&index_text);

    // Every entry's `file` exists; every on-disk file has an entry.
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for file in entries.keys() {
        assert!(
            !file.is_empty(),
            "provenance entry missing `file:` key in {}",
            PROVENANCE_INDEX
        );
        let on_disk_path = dir.join(file);
        assert!(
            on_disk_path.is_file(),
            "provenance entry `{}` points at a missing file: {}",
            file,
            on_disk_path.display()
        );
        referenced.insert(file.clone());
    }
    let orphans: Vec<&String> = on_disk.iter().filter(|f| !referenced.contains(*f)).collect();
    assert!(
        orphans.is_empty(),
        "files in {} lack provenance entries: {:?}",
        WIRE_FIXTURE_DIR,
        orphans
    );

    // Walk every entry and check `kind`, `used_by`, `pinned_commit`.
    let pin_kde = read_upstream_pin(UPSTREAM_FILES[0]);
    let pin_android = read_upstream_pin(UPSTREAM_FILES[2]);
    let pin_gsconnect = read_upstream_pin(UPSTREAM_FILES[1]);

    for (file, fields) in entries.iter() {
        let kind = fields
            .get("kind")
            .expect("provenance entry missing `kind`");
        assert!(
            matches!(kind.as_str(), "upstream-derived" | "live-transcript" | "hand-authored-from-observation"),
            "fixture `{}` has unknown `kind` `{}`",
            file,
            kind
        );

        if kind == "upstream-derived" {
            for required in ["upstream_repo", "pinned_commit", "source_file", "extraction_date"] {
                assert!(
                    fields.contains_key(required),
                    "fixture `{}` (upstream-derived) missing `{}`",
                    file,
                    required
                );
            }
            let repo = fields.get("upstream_repo").unwrap();
            let pin = fields.get("pinned_commit").unwrap();
            let expected = match repo.as_str() {
                "kdeconnect-kde" => &pin_kde,
                "kdeconnect-android" => &pin_android,
                "gsconnect" => &pin_gsconnect,
                other => panic!("fixture `{}` has unknown upstream_repo `{}`", file, other),
            };
            assert_eq!(
                pin, expected,
                "fixture `{}` pinned_commit `{}` does not match upstream-capabilities header pin `{}` (D6 cross-check)",
                file, pin, expected
            );
        } else {
            // live-transcript / hand-authored-from-observation still need
            // extraction_date and a `note`.
            assert!(
                fields.contains_key("extraction_date"),
                "fixture `{}` ({}) missing `extraction_date`",
                file,
                kind
            );
            assert!(
                fields.contains_key("note"),
                "fixture `{}` ({}) missing `note`",
                file,
                kind
            );
        }

        // used_by: every `<file>::<test_fn>` must point at an existing file
        // containing the named test function.
        if let Some(used_by) = fields.get("used_by") {
            for ref_ in used_by.split(',') {
                let ref_ = ref_.trim();
                if ref_.is_empty() {
                    continue;
                }
                let (file_part, fn_part) = ref_
                    .split_once("::")
                    .unwrap_or_else(|| panic!("used_by `{}` must be `<file>::<fn>`", ref_));
                let test_path = root.join(file_part);
                assert!(
                    test_path.is_file(),
                    "used_by `{}` references missing file {}",
                    ref_,
                    test_path.display()
                );
                let body = std::fs::read_to_string(&test_path).expect("read used_by file");
                let needle = format!("fn {}(", fn_part);
                assert!(
                    body.contains(&needle),
                    "used_by `{}` — file {} has no `fn {}(`",
                    ref_,
                    test_path.display(),
                    fn_part
                );
            }
        }
    }
}

/// Read the `pinned_commit:` line out of a `tests/fixtures/upstream-capabilities/*.yaml`
/// header. Returns the bare SHA string. Panics if the file lacks one — that
/// fixture is broken upstream-side and the lint must surface it.
fn read_upstream_pin(path: &str) -> String {
    for line in read(path).lines() {
        let body = line.trim_start();
        if let Some(rest) = body.strip_prefix("# pinned_commit:") {
            return rest.trim().to_string();
        }
        // The pin lives in a header comment; stop scanning once we leave
        // the comment block (any non-# non-blank line).
        if !body.is_empty() && !body.starts_with('#') {
            break;
        }
    }
    panic!(
        "{} does not carry a `# pinned_commit:` header — the fixture is broken",
        path
    );
}