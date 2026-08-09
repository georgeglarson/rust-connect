# Task 1.7 report — close advertised control-surface gaps

Branch: `feat-task-1.7-control-surface` (worktree `~/repos/rust-connect-feat-task-1.7`, off `1d87836`)

Commits, in order:

| Instance | Commit | Subject |
|---|---|---|
| 4 — `/api/v1/tools` catalog | `bf1d64f` | fix(plugins): wire is_backend_available for pausemusic/sendnotifications/screensaver-inhibit |
| Phase 1 — dead-knob lint | `ab1b9bd` | test(config): add the dead-knob lint |

Instances 1–3 needed no commit — re-audit found them already fixed (see below). The route-table lint (the other Phase 1 artifact) needed no commit either — it already exists and already passes (`tests/route_table_lint.rs`).

## Headline finding

**The audit is more stale than the team-lead's heads-up suggested.** Not one instance (clipboard/request) but *three of the four* named audit instances were already fully resolved by the time this session started. Only the `/api/v1/tools` catalog instance had a real, still-live gap, and it was narrower than the audit's framing — the generic mechanism it describes already existed and was already correctly wired for four plugins; three more just hadn't been given the same one-line override yet.

## Instance 1 — `POST /devices/{id}/clipboard/request`

**Audit claim:** handler + OpenAPI annotation + UI button exist, but no router entry — 404 today.

**Re-audit finding:** STALE. `src/api/router.rs:82-84` has the route wired to `handlers::request_clipboard`. The handler (`src/api/handlers/plugins/clipboard.rs:98-121`) and its `#[utoipa::path]` annotation (`:84-97`) both exist and match. `tests/api_plugin_endpoints.rs:468-490` (`test_clipboard_request_route_exists`) already exercises it end-to-end, asserting a non-404 status — the exact same pattern (existence via "not 404", since the test device is registered but not connected) used by 9 sibling `_route_exists` tests in the same file for other `/request`-shaped routes. This is the codebase's established convention for this route class, not a gap.

**Action:** none. Already fixed, already tested, matching precedent.

## Instance 2 — `settings.udp_port` / `--port` flag

**Audit claim:** accepted but never read; discovery binds the port constant.

**Re-audit finding:** STALE. Traced the full chain:
- `src/cli/mod.rs:48-49`: `--port` (`#[arg(short, long)]`) → `Cli.port: Option<u16>`.
- `src/bootstrap.rs:44-45`: `load_config` sets both `settings.tcp_port` and `settings.udp_port` from it.
- `src/services/service_manager.rs:93-100`: `start_discovery` passes `state.settings.udp_port` (and `broadcast_interval_secs`) into `DiscoveryService::new`.
- `src/protocol/discovery.rs:61-104`: `DiscoveryService::new` binds the listen socket AND sets `broadcast_addr` to the SAME `udp_port` parameter — both legs, not just one.
- `src/protocol/discovery.rs:484-509`: `test_new_binds_and_broadcasts_on_configured_port` already pins exactly this (configured port ≠ default, asserts both the bound socket's port and `broadcast_addr`'s port equal it).
- The related, tightly-coupled question ("what the identity packet advertises"): `settings.tcp_port` follows the identical pattern — `service_manager.rs:38` binds `TcpListenerService::bind_port(state.settings.tcp_port)`, and `:50` sets `identity.tcp_port = Some(actual_port)` (the ACTUAL bound port, not a hardcoded constant) before it goes out on the wire.

**Action:** none. Already fixed, already tested, both discovery legs and the identity advertisement all correct.

## Instance 3 — `settings.protocol_version`

**Audit claim:** accepted but never read.

**Re-audit finding:** STALE — and further along than "wire it": the field has ALREADY BEEN DELETED from `AppSettings`. Confirmed by reading the full struct (`src/config/settings.rs:21-44`) — no `protocol_version` field exists. `src/config/settings.rs:439-456` (`test_load_ignores_legacy_protocol_version_field`) is a regression test with a doc comment stating exactly the brief's prescribed fix: a config file carrying the legacy key must still load (serde ignores unknown fields) and settings must equal defaults. Grepped `README.md`, `docs/` (excluding `docs/reference/` and `docs/archive/`, which are upstream KDE Connect source snapshots where `protocolVersion` is the real, legitimate WIRE field, not this setting), and `src/api/` for any lingering mention of a settable `protocol_version` config option: none found. Nothing to scrub.

Note: the wire concept `identity.protocol_version` / `protocolVersion` (a completely different thing — the KDE Connect protocol version each device advertises in its identity packet) is alive and correctly used throughout `src/protocol/`, `src/device/`, `src/services/` — that was never in scope; only the deleted `AppSettings.protocol_version` config field was.

**Action:** none. Already fixed, already tested, nothing to scrub.

## Instance 4 — `/api/v1/tools` catalog vs backend availability

**Audit claim:** the catalog lists plugins whose backend failed to init.

**Re-audit finding:** PARTIALLY STALE. The generic mechanism the claim describes already exists: `Plugin::is_backend_available()` (`src/plugins/plugin.rs:35-37`, default `true`) and `list_tools`'s use of it (`src/api/handlers/plugins/mod.rs:290-304`) to mark a catalog entry `available: false`. It was already correctly overridden for `clipboard`, `mpris`, `systemvolume`, and `sftp`. `list_tools`'s own comment named the gap precisely: *"a generic hook covers any future backend-bearing plugin (sendnotifications, pausemusic, screensaver_inhibit) without per-plugin special cases"* — those three never actually got the override, so they always reported available regardless of real backend state.

**Action (`bf1d64f`):** added `is_backend_available()` to all three, same `self.backend.read().map(|b| b.is_some()).unwrap_or(false)` pattern as the existing four:
- `pausemusic.rs`: reflects the primary MPRIS pause backend (`self.backend`; the mute leg's `volume_backend` has no wire capability of its own to attach a separate availability signal to).
- `screensaver_inhibit.rs`: reflects the injected `ScreensaverBackend`.
- `sendnotifications.rs`: no swappable `Option<Arc<dyn Backend>>` here — `watcher_started` (the field `try_start_watcher` already sets/clears) is the honest signal instead.

**Red before green:** new code, no prior test to naturally turn red. Temporarily removed all three overrides (falling back to the trait default) and re-ran — all three new tests failed exactly as predicted, each on `assertion failed: !plugin.is_backend_available()`. Reverted; full suite green again.

**Caveat, found during verification, worth stating plainly:** the fix is CORRECT but only PARTIALLY OBSERVABLE through `/api/v1/tools` today:
- `screensaver_inhibit`'s incoming capabilities are `vec![]` — it produces zero catalog entries either way, so the override is currently inert there (still correct on principle, future-proofs if it ever gains a capability).
- `sendnotifications`'s incoming capability is `"kdeconnect.notification.request"`, which is not one of the 11 keys `capability_to_tool` maps (`src/api/handlers/plugins/mod.rs:71-219`) — also zero catalog entries, also currently inert for this one consumer.
- `pausemusic`'s incoming capability, `"kdeconnect.telephony"`, IS mapped — but so is `telephony.rs`'s own `incoming_capabilities()`, which declares the SAME string. `list_tools` iterates `state.plugin_registry.list_with_capabilities()`, backed by a plain `HashMap<String, Arc<dyn Plugin>>` (`src/plugins/registry.rs:24`) with non-deterministic iteration order — so `/api/v1/tools` pushes TWO entries both named `"get_telephony"`, and which one a client observes (with which `available` value) is not deterministic run-to-run.

None of these three are a "wire or remove, smallest honest change" — the first two would mean inventing new catalog surface for plugins that were never advertised as tools at all; the third needs a real decision (dedupe by plugin+capability? rename one? merge semantics?) that is API-shape work, not a lane-level fix. Recording all three here per the brief; none fixed in this branch.

## Sweep beyond the audit's list (Phase 0 item 5)

- Cross-checked every plugin with a degradable `Option<Arc<dyn Backend>>`-shaped field (`clipboard`, `mpris`, `systemvolume`, `sftp`, `pausemusic`, `screensaver_inhibit`, `findthisdevice`) against whether it overrides `is_backend_available()`. `findthisdevice`'s `backend` field is `Some(...)` unconditionally from construction (never `None` — its "unavailable" state is a per-call fact checked inside `ring()`, not a persistent connection state), so it correctly needs no override; every other plugin in that set now has one.
- The `pausemusic`/`telephony` `"kdeconnect.telephony"` capability-string collision above (found while chasing instance 4, not separately searched for).
- Ran both Phase 1 lints (`route_table_lint`, `dead_knob_lint`) as the sweep tool the brief describes — both currently report zero discrepancies beyond what's covered above.

## Phase 1 — durable artifacts

**Route-table lint:** ALREADY EXISTS (`tests/route_table_lint.rs`, 222 lines) and ALREADY PASSES. It does exactly what the brief specifies: parses `.route("...")` calls out of `src/api/router.rs`'s source text (axum has no stable runtime route-introspection API in this version), normalizes axum's `:param` syntax to OpenAPI's `{param}` syntax, and asserts set equality against `ApiDoc::openapi()`'s path keys — modulo a small, commented allowlist (`/`, `/ui`, `/ui/`, `/ui/index.html` — UI plumbing, never OpenAPI-annotated by design; `/api/v1/events` — the SSE channel, which utoipa can't model). A second test (`test_ui_endpoints_are_wired`) extracts `/api/v1/...` string literals out of `src/api/ui/index.html` and asserts every one is a real router path — the brief's named fallback for "if [UI coverage is] only greppable HTML/JS, a best-effort extraction with a maintained list is acceptable," already documented as the chosen approach in the file's own module doc.

**Dead-knob test:** did not exist. Built (`ab1b9bd`, `tests/dead_knob_lint.rs`) as the brief's "disciplined test-with-list": `AppSettings` derives `Serialize` with no `#[serde(skip)]`/`#[serde(rename)]` anywhere on it, so serializing a `Default()` instance's JSON keys equal its field names exactly. A maintained `KNOWN_FIELDS` list (15 entries, one per current field, each with a file:line reader citation gathered and verified by hand this session) is diffed against that key-set in both directions — an unregistered new field fails loudly, and so does a stale `KNOWN_FIELDS` entry for a field that got deleted. A second test pins the struct has no skip/rename attributes, since either would break the serialize-and-diff technique's core assumption silently. Red-before-green: injected a genuinely dead field, confirmed the exact intended failure message, reverted.

## Gates (all green, at `ab1b9bd`)

- `cargo test --locked` — 929 lib unit tests + every integration suite (incl. the new `dead_knob_lint`, the pre-existing `route_table_lint`, and `test_clipboard_request_route_exists`/`test_list_tools_marks_degraded_backends` in `tests/api_plugin_endpoints.rs`), 0 failed.
- `cargo clippy --all-targets --locked -- -D warnings` — clean.
- `cargo fmt --check` — clean.

## Deferred items (need integrator judgment, not a lane fix)

1. **`pausemusic`/`telephony` `"kdeconnect.telephony"` capability collision** (above) — produces two same-named, non-deterministically-ordered `/api/v1/tools` entries. Needs a decision on dedup semantics; API-shape change, out of this branch's "smallest honest change" boundary.
2. **`sendnotifications` and `screensaver_inhibit` have no `/api/v1/tools` catalog entry at all**, capability-string or empty-capability reasons respectively (above). The `is_backend_available()` fix is correct and tested regardless, but whether these plugins SHOULD be catalog-visible tools is a product decision, not something this branch invented an answer to.
3. No wire-behavior change was needed for `udp_port` (it was already correctly wired), so the brief's "STOP and flag if wiring changes wire behavior" boundary never triggered — noting it explicitly since the brief called it out as a live risk.
