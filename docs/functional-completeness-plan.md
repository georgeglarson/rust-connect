# Plan: Functional completeness and evidence closure

**Generated:** 2026-08-05
**Amended:** 2026-08-05, after a three-lane audit (MiniMax-M3 claim
verification + gap sweep, GLM-5.2 adversarial review; reports in
`plans/audit-2026-08-05-plan-review/`, verdict in the vault project page)
**Estimated complexity:** High
**Canonical scope:** This file defines the work. `ROADMAP.md` summarizes it.

## Overview

Rust Connect has repeatedly reached "all gaps closed" by completing the gaps an
audit happened to enumerate. That is not a stable completeness claim. The current
behavioral parity checklist explicitly excludes plugin-level packet shapes, and the
live matrix covers only a subset of the advertised capabilities on one Linux desktop
environment.

This plan replaces checklist closure with evidence closure. A feature is complete
only when its row in `docs/functional-coverage.md` proves the relevant dimensions:

1. upstream wire contract;
2. real desktop or phone effect;
3. API/CLI reachability when the feature is controllable;
4. lifecycle cleanup and reconnect behavior;
5. malformed-input and authorization behavior;
6. automated evidence using upstream-derived fixtures or a real peer;
7. live evidence on available hardware when practical;
8. environment evidence for platform-dependent backends.

`UNVERIFIED` is a first-class status. Missing evidence cannot be recorded as
conformant, and an advertised capability whose backend silently degrades is not
functionally complete.

## Scope and priority rules

- Required: compatibility with the stock KDE Connect Android app; security,
  recovery, and long-running stability; honest capability advertisement; a real
  Linux desktop effect for advertised desktop-provider features.
- Validate now: Samsung A15 and S21. Add a Pixel or other Android implementation
  through a volunteer when available; lack of a device records `UNVERIFIED`, not
  `PASS` or a blocker by itself.
- Nice to have: the union of kdeconnect-kde and GSConnect behavior, including
  desktop-to-desktop features that the Android client cannot exercise.
- Release reach, not core completeness: additional package formats, polished tray
  UI, non-systemd support, immutable distributions, and macOS.
- Environment correctness is still required: supported functionality must be
  exercised on Sway, GNOME, KDE Plasma, Wayland, and X11 where applicable. A VM is
  acceptable for desktop integration; real hardware is preferred for input,
  suspend/resume, networking, and media backends.

## Prerequisites

- Rust Connect checkout and green baseline gates: `cargo test --locked`, clippy,
  rustfmt, cargo-deny, fuzz smoke, and release build.
- Pinned source revisions for kdeconnect-android, kdeconnect-kde, and GSConnect.
- A15 and S21 with the stock Android client; ADB where useful, but assertions must
  measure the user-visible result rather than merely packet transmission.
- Disposable GNOME/KDE/X11 test sessions or VMs. Do not alter the primary Sway
  session merely to create coverage.
- Network namespaces for peer and fault-injection tests; root-only tests remain an
  explicit on-demand suite rather than silently skipped CI coverage.

## Execution routing (2026-08-05 amendment)

This plan is executed through a delegation stack: a strong model holds the
knife — writes the briefs, owns the gates, integrates, and verifies — while
flat-rate subscriptions execute the lanes. The task format above
(Location / Dependencies / Acceptance / Validation) is the executor brief
format; a seven-sprint cheaper-model execution in this repo's history ran
clean and inspected faithful under exactly this shape.

- **Big implementation lanes** — MiniMax M3, headless Claude Code on the
  provider's Anthropic-compatible endpoint: Tasks 1.1, 1.2, 1.3, 1.4, 1.5,
  1.6, 1.7, 2.1, 2.3. Well-specified builds with clear acceptance criteria.
  One lane per task; no two concurrent lanes touch overlapping file sets.
- **Bulk/mechanical lanes** — MiniMax M3 or MiMo: Task 0.1 inventory
  fixtures, Task 0.4 self-referential test conversion (worklist is already in
  the task), Task 4.2 device-matrix scripting support, Task 5.4 doc scrub.
- **Adversarial/review lanes** — GLM-5.2: Task 0.5 black-box audit, the
  independent semantic scan in Task 2.5, disputed ledger rows, Task 3.1
  role classification. A reviewer must come from a different provider than
  the lane under review.
- **Rig work, not token work**: Sprint 4 environments, soaks, and Task 4.5
  install walks are shell + supervision; they cost attention, not quota.
- **Never delegated**: live-phone validation (A15 via scripted adb taps, S21
  by hand), merges and pushes, Task 5.2 announcement wording, and the
  AI-provenance decision.

Standing executor discipline — earned lessons; restate in every brief:

- The executor never pushes and never merges; the integrating session owns
  git state, and push discipline is re-stated in every brief.
- Briefs mandate red-before-green tests and upstream file:line citations;
  fixtures come from upstream source, never from this repo's own structs.
- The integrator verifies the tree, never the executor's summary.
- One cargo build at a time, and never with target directories on tmpfs.
- Live validation against the phones is the integrator's job; the phone is
  the oracle.

## Sprint 0: Make completeness falsifiable

**Goal:** Establish an exhaustive inventory and a gate that cannot declare success
from an incomplete checklist.

**Demo/validation:** A generated coverage document accounts for every capability
advertised by Rust Connect and every current upstream plugin/capability. Removing a
Rust plugin or adding a fixture capability makes the coverage check fail.

**Sequencing amendment (2026-08-05).** Sprint 0 runs as two slices so Sprint 1
does not wait on the full machinery:

- **Slice 0A (thin, first):** Task 0.1 (pinned upstream inventory), Task 0.2
  (Rust inventory generated from the loader), and the ledger skeleton with the
  status vocabulary (Task 0.3). Enough to see and prioritize every gap.
- **Slice 0B (then, in parallel with Sprint 1):** the self-referential-test
  pass (Task 0.4), the black-box audit (Task 0.5), and the lint/mutation gates
  (Task 0.3 validation).

Sprint 1 tasks depend on Slice 0A only — their gaps are known from current
source and do not require the full ledger machinery to start.

### Task 0.1: Pin all three upstream inventories

- **Location:** `tests/fixtures/upstream-capabilities/`,
  `docs/functional-coverage.md` (refresh script under `tools/` optional)
- **Description:** Record plugin names, incoming/outgoing packet types, and the
  exact source revision of kdeconnect-android, kdeconnect-kde, and GSConnect as
  committed, normalized fixtures. A hand-built, cited inventory is sufficient;
  automatic multi-language extraction is deferred unless the drift review
  (Task 5.3) proves it unmanageable. Do not depend on a developer's absolute
  checkout path.
- **Dependencies:** None.
- **Acceptance criteria:** Every fixture entry cites its upstream file and pinned
  SHA; the completeness test fails when a Rust plugin or fixture capability has
  no ledger row; diffs expose upstream additions/removals at refresh time.
- **Validation:** Mutate one fixture and watch the inventory test fail; refresh
  from the pinned upstreams and confirm a clean re-run.

### Task 0.2: Generate the Rust capability inventory from production wiring

- **Location:** `src/plugins/loader.rs`, `tests/check_caps.rs`,
  `tools/render-functional-coverage.rs`
- **Description:** Enumerate production-registered plugins and both capability
  directions from the actual loader. Include lifecycle-only plugins with no packet
  types. Do not maintain a second handwritten Rust roster.
- **Dependencies:** Task 0.1.
- **Acceptance criteria:** Every production plugin appears exactly once; tests fail
  when loader registration and the coverage ledger diverge.
- **Validation:** Mutation-check one registration and one advertised packet type.

### Task 0.3: Create the evidence ledger and status vocabulary

- **Location:** `docs/functional-coverage.md`, `docs/live-validation.md`
- **Description:** Three small matrices instead of one wide one, so the ledger
  stays maintainable by one person:
  1. **Feature ledger** — one row per feature/role (not merely per module), with
     columns for the eight evidence dimensions (upstream refs, desktop/phone
     effect, API/CLI surface, lifecycle/recovery, hostile-input behavior,
     fixture provenance, live-device evidence, environment evidence) plus status.
  2. **Environment matrix** — keyed by the backends that actually vary by
     desktop (clipboard-X11/Wayland, uinput, audio, session D-Bus, notification
     server), not one column per DE per feature.
  3. **Device matrix** — A15, S21, other Android (volunteer).

  Use `PASS`, `FAIL`, `UNVERIFIED`, `NOT-APPLICABLE`, and
  `INTENTIONAL-DIVERGENCE`; every non-pass state needs a reason and owner task.
- **Dependencies:** Tasks 0.1–0.2.
- **Acceptance criteria:** The old protocol checklist, all Rust plugins, and all
  upstream-only capabilities are represented. No prose claim can substitute for a
  row's evidence links. An effect `PASS` requires a peer-side or phone-side
  artifact — our own log line or API response does not count ("packet-sent is
  never receipt evidence", enforced the same way as wire conformance).
- **Validation:** A schema/lint test rejects missing rows, unknown status values,
  uncited `PASS`, an effect `PASS` without a peer-side artifact, and expired
  upstream revisions.

### Task 0.4: Eliminate self-referential wire-conformance evidence

- **Location:** `tests/`, module-local tests, `docs/functional-coverage.md`
- **Description:** A targeted pass, not an exhaustive relabeling: find every
  wire-conformance test whose fixture is produced by serializing this repo's own
  structs (or hand-typed against them) and convert it to an upstream-derived
  literal fixture or an independent-peer check. The defect class that repeatedly
  bit this project is narrow — wire shapes certified by Rust-to-Rust agreement —
  so only wire-conformance tests need provenance labels; other unit tests do not.
  Known instances at audit time (fix or justify each):
  `tests/protocol_integration.rs:49`
  (`test_identity_packet_format_matches_kde_connect`),
  `src/protocol/types.rs:567`
  (`test_packet_payload_fields_serialize_camel_case`), the six `*_wire_shape`
  tests in `src/plugins/mpris/mod.rs`, `src/plugins/clipboard.rs:841`
  (`test_local_change_packet_wire_shape`), `src/api/handlers/share.rs:572`, and
  the runcommand `_wire_shape` trio in `src/plugins/runcommand.rs`.
- **Dependencies:** Task 0.3.
- **Acceptance criteria:** Every wire-conformance `PASS` has at least one
  non-self-referential source for its wire shape.
- **Validation:** Coverage lint refuses a wire-conformance `PASS` backed only by a
  Rust-self test; each converted test cites its upstream fixture source.

### Task 0.5: Run one independent black-box gap audit

- **Location:** `docs/audits/functional-gap-audit-YYYY-MM-DD.md`
- **Description:** One black-box audit starting from the published binary/API —
  what a skeptic does — without trusting internal docs. (The source-diff audit
  from the original plan is cut: three prior multi-lane source audits already
  covered that surface, and Task 0.1's pinned inventory fixtures keep it covered
  going forward.) Reproduce each finding before promoting it into the ledger or
  backlog.
- **Dependencies:** Tasks 0.1–0.4.
- **Acceptance criteria:** Auditors use the full ledger boundary; disputed findings
  remain `UNVERIFIED` with a proposed experiment.
- **Validation:** Audit report maps every finding to a coverage row or explicitly
  records why it is rejected.

## Sprint 1: Close advertised-but-incomplete functionality

**Goal:** Anything Rust Connect advertises to an Android peer — and anything the
REST API, CLI, or web UI offers to a local caller — produces the promised effect,
or is withheld/removed when its backend is unavailable.

**Demo/validation:** On the A15 and S21, each applicable advertised capability has a
user-visible result and recovery check. Starting the daemon without a required
backend produces honest capability negotiation and a diagnostic. Every documented
API route, UI control, and config option either works or does not exist.

**Dependencies for all tasks below: Slice 0A only** (see Sprint 0 sequencing) —
these gaps are known from current source and do not wait on the full ledger
machinery.

### Task 1.1: Implement the local system-volume provider

- **Location:** `src/plugins/systemvolume.rs`, new backend module under
  `src/plugins/systemvolume/`, `src/bootstrap.rs`, API volume handlers
- **Description:** Add PipeWire/PulseAudio sink discovery, full-state publication,
  delta events, volume/mute/default-sink commands, and reconnect supervision. Keep
  the existing remote-controller role distinct.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Phone lists real local sinks and changes volume/mute;
  desktop changes propagate back; no provider capability is advertised when no
  supported backend exists.
- **Validation:** Backend contract tests, session integration test, A15/S21 live
  test, PipeWire restart and device hot-plug test.

### Task 1.2: Make run-command usable without a code-only API

- **Location:** `src/plugins/runcommand.rs`, `src/config/`, API handlers,
  `docs/threat-model.md`
- **Description:** Add a persisted, per-device allowlist with validation, safe
  reload, explicit shell semantics, timeout/process-tree termination, setup/stop
  behavior where supported, and output streaming if advertised.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Production can configure and run a command from the
  phone; unknown commands fail closed; output capability is advertised only when
  fully implemented.
- **Validation:** Upstream-derived request fixtures, authorization tests, timeout
  and descendant-process tests, A15/S21 invocation with observable output.

### Task 1.3: Complete SFTP browsing as a desktop feature

- **Location:** `src/plugins/sftp.rs`, new mount backend, API handlers,
  `docs/troubleshooting.md`
- **Description:** Turn received credentials into a bounded-lifetime mount or an
  explicit open-in-file-manager action. Clean up on disconnect, unpair, daemon exit,
  and credential rotation. Never expose credentials in logs or API history.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** A user can browse phone storage from the desktop; stale
  mounts and credentials are removed on every lifecycle exit.
- **Validation:** Mock SFTP server, disconnect/crash cleanup, A15/S21 file browse,
  copy, reconnect, and unpair tests.

### Task 1.4: Complete notification presentation and actions

- **Location:** `src/plugins/notification.rs`, `src/plugins/sendnotifications.rs`,
  API notification handlers
- **Description:** Implement icon payloads, inline actions, reply/dismiss identity,
  initial-state synchronization, replacements, and cancellation without duplicate
  popups. Confirm behavior against current Android source rather than stored Rust
  fixtures.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Mirrored notifications retain icon and available actions;
  reply/dismiss reaches the originating notification; resync is idempotent.
- **Validation:** Upstream payload fixtures plus A15/S21 tests using apps with real
  RemoteInput actions.

### Task 1.5: Complete MPRIS payload and lifecycle behavior

- **Location:** `src/plugins/mpris/`, `src/protocol/payload_transfer.rs`
- **Description:** Add album-art payload support, player add/remove races, metadata
  changes, seek/set-position units, volume, and session-bus recovery.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Phone displays album art and controls real players; stale
  players disappear; restarting the media player or D-Bus restores service without
  restarting Rust Connect.
- **Validation:** Live MPRIS integration with two players, upstream request fixtures,
  A15/S21 control tests.

### Task 1.6: Close smaller advertised backend gaps

- **Location:** `src/plugins/mousepad.rs`, `src/plugins/pausemusic.rs`,
  `src/plugins/clipboard.rs`, `src/plugins/findthisdevice.rs`
- **Description:** Implement mousepad absolute axes, decide and document pausemusic
  mute behavior, add an X11 clipboard backend, and verify ringtone/audio fallback
  behavior. Split into one commit per backend.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Each advertised path has a real backend on its supported
  environment; unsupported environments withhold or explicitly mark the capability.
- **Validation:** evdev event capture, live call/media test, X11 clipboard roundtrip,
  audio-backend restart tests.

### Task 1.7: Close advertised control-surface gaps

- **Location:** `src/api/router.rs`, `src/api/ui/`, `src/config/settings.rs`,
  `src/api/handlers/plugins/`
- **Description:** Wire or remove every documented control. Audit-time instances:
  `POST /devices/{id}/clipboard/request` has a handler, an OpenAPI annotation,
  and a web-UI button but no router entry (404 today); `settings.udp_port` (the
  `--port` flag) and `settings.protocol_version` are accepted but never read —
  discovery binds the port constant; the `/api/v1/tools` catalog lists plugins
  whose backend failed to init. The SFTP mount/unmount UI buttons are owned by
  Task 1.3.
- **Dependencies:** Slice 0A.
- **Acceptance criteria:** Every route in the OpenAPI spec is reachable and
  exercised by a test; every config option and CLI flag either changes behavior
  or is deleted; the tools catalog reflects backend availability.
- **Validation:** Route-table lint comparing router entries against OpenAPI paths
  and UI actions; a dead-knob test fails when a setting has no reader.

## Sprint 2: Close known protocol, recovery, and security gaps

**Goal:** Resolve the nine documented behavioral gaps and test the failure modes
that ordinary happy-path suites miss.

**Demo/validation:** Android and kdeconnectd survive network changes, blackholes,
malformed peers, interrupted transfers, suspend/resume, and repeated replacement
connections without stale state or resource growth.

### Task 2.1: Fix the robustness trio

- **Location:** `src/protocol/payload_transfer.rs`, `src/device/registry.rs`,
  `src/protocol/discovery.rs`
- **Description:** Align payload accept timeout, preserve known capabilities on an
  empty-cap identity, and raise/test the UDP receive capacity.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Existing Vikunja #997 scenarios pass with upstream-sized
  fixtures and resource bounds.
- **Validation:** Red-before-green tests for all three plus oversized live identity
  injection.

### Task 2.2: Make discovery event-driven and roaming-safe

- **Location:** `src/services/service_manager.rs`, `src/protocol/discovery.rs`,
  `src/protocol/mdns_discovery.rs`
- **Description:** Add network-change re-announcement, complete the mDNS soak, remove
  broadcast-forever behavior, and implement bounded fallback when discovery methods
  fail.
- **Dependencies:** Task 2.1.
- **Acceptance criteria:** Discovery recovers across Wi-Fi roam, interface loss,
  address change, suspend/resume, and mDNS failure without periodic redial churn.
- **Validation:** Network-namespace tests plus A15/S21 roaming soak; closes #994.

### Task 2.3: Implement remaining KDE behavioral edges

- **Location:** protocol discovery, packet, payload, and send paths named in
  `docs/parity-checklist.md`
- **Description:** Add reverse-connection fallback, oversized-identity empty-cap
  retry, `payloadSize=-1` streaming with explicit resource limits, and send-side
  capability gating.
- **Dependencies:** Tasks 2.1–2.2.
- **Acceptance criteria:** Each divergence has a reference-derived test and an
  intentional policy for the Android/KDE disagreement; closes #998.
- **Validation:** kdeconnectd peer harness and adversarial streaming tests.

### Task 2.4: Exercise dead-link and replacement recovery

- **Location:** network-namespace integration tests, connection lifecycle modules
- **Description:** Add the keepalive blackhole test, duplicate-dial storms, delayed
  stale-loop cleanup, peer restart, daemon restart, and suspend/resume.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Exactly one live generation remains; state converges;
  no redial storm or stale disconnect event; closes #990.
- **Validation:** Root-only fault suite with bounded deadlines and resource counts.

### Task 2.5: Run a focused security and resource-lifetime audit

- **Location:** TLS/pairing, payload, API auth/CORS/rate limits, file handling,
  command execution, input injection, persistent state
- **Description:** Audit unauthenticated LAN paths and paired-but-malicious peers.
  Cover CPU/memory/disk/fd/task bounds, path handling, secret exposure, certificate
  rotation, replay, and authorization across every API operation. Additionally:
  (a) cross-check upstream KDE Connect CVE/advisory history for protocol-level
  vulnerabilities a reimplementation inherits; (b) verify `devices.json` pruning
  against the real store — `prune_stale_devices` (`src/device/registry.rs`)
  drops stale *unpaired* records, and the reinstall-duplicate pattern (dead
  device ids surviving under one device name) may be paired-or-persisted records
  it does not catch; (c) classify the single-key, no-scopes API authorization
  model as an intentional divergence with a recorded reason, or scope it.
- **Dependencies:** Sprint 0; run again after Tasks 1.2–1.5.
- **Acceptance criteria:** Every finding has a reproducer; high/critical findings are
  fixed before the next release; lower risks remain explicit ledger entries.
- **Validation:** Hostile-peer suite, fuzz corpus, dependency audit, advisory
  cross-check note, and independent semantic scan with findings treated as leads
  rather than truth.

## Sprint 3: Account for the full upstream feature union

**Goal:** Make every feature in current kdeconnect-kde and GSConnect either
implemented or explicitly classified as optional parity debt with a tested reason.

**Demo/validation:** The upstream inventory has no unclassified rows. Rust Connect
pairs with kdeconnectd/GSConnect in addition to Android.

### Task 3.1: Audit apparently missing KDE plugin roles

- **Location:** `docs/functional-coverage.md`, `src/plugins/`
- **Description:** Examine remotecontrol, shareinputdevices,
  shareinputdevicesremote, virtualmonitor, and any upstream additions discovered by
  Task 0.1. Distinguish missing behavior from differently named/combined Rust
  modules.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Each upstream role maps to Rust code, an implementation
  task, or a documented nice-to-have decision with no dishonest capability claim.
- **Validation:** Inventory completeness test.

### Task 3.2: Build the kdeconnectd independent-peer harness

**Announcement-critical (2026-08-05 amendment).** Desktop peers exercise the
desktop-provider direction — clipboard, mpris, runcommand, sendnotifications,
remotekeyboard, and the Task 1.1 volume provider — that no Android phone ever
touches. Any public claim broader than Android-core requires this harness to
have run; do not let it drift behind the announcement.

- **Location:** `tests/interop/`, test scripts, CI/on-demand documentation
- **Description:** Run kdeconnectd and Rust Connect in separate network namespaces;
  cover discovery, pairing, reconnect, clipboard, share, notifications, MPRIS,
  commands, and applicable desktop-to-desktop roles.
- **Dependencies:** Sprint 0 and relevant Sprint 1 features.
- **Acceptance criteria:** Wire assertions observe the other implementation, not a
  Rust peer; closes #991.
- **Validation:** Reproducible one-command on-demand harness with pinned KDE SHA.

### Task 3.3: Add GSConnect interoperability coverage

- **Location:** `tests/interop/`, `docs/functional-coverage.md`
- **Description:** Exercise GSConnect as an independent peer in a disposable GNOME
  session. Prioritize capabilities where GSConnect differs from KDE.
- **Dependencies:** Task 3.2 and GNOME environment from Sprint 4.
- **Acceptance criteria:** Every GSConnect feature is classified and core shared
  flows pass in both directions.
- **Validation:** Recorded GNOME-session run with GSConnect revision and artifacts.

## Sprint 4: Validate devices, desktops, and long-running operation

**Goal:** Prove the implementation outside the development machine's exact shape.

**Demo/validation:** A published matrix identifies tested combinations and honest
unknowns. A multi-day daemon soak finishes without resource drift or lost recovery.

### Task 4.1: Create disposable desktop test environments

- **Location:** `tests/environments/`, `docs/testing.md`
- **Description:** Define Fedora/Sway, GNOME Wayland, KDE Plasma Wayland, and one X11
  session. Capture required portals, D-Bus services, audio stack, notification
  server, clipboard utilities, uinput access, and systemd-user behavior.
- **Dependencies:** Sprint 0.
- **Acceptance criteria:** Each environment can install/start the release artifact,
  expose diagnostics, and run the environment suite reproducibly.
- **Validation:** Fresh environment rebuild followed by smoke suite.

### Task 4.2: Run the full A15 and S21 matrices

- **Location:** `docs/live-validation.md`, `docs/functional-coverage.md`, live tests
- **Description:** Exercise both connection directions and every applicable Android
  plugin on both phones. Record Android version, KDE Connect version, network,
  desktop environment, result, and observable evidence.
- **Dependencies:** Sprints 1–2.
- **Acceptance criteria:** Every Android-app row is `PASS`, `FAIL`, or explicitly
  `UNVERIFIED` with why; packet-sent alone is never receipt evidence.
- **Validation:** Artifacts include API output, daemon logs, phone-side observation,
  and hashes/content checks where applicable.

### Task 4.3: Recruit a non-Samsung validation device

- **Location:** issue template or tester instructions, coverage ledger
- **Description:** Prepare a minimal volunteer script for a Pixel or another OEM,
  avoiding secrets and collecting only needed environment/version facts.
- **Dependencies:** Task 4.2.
- **Acceptance criteria:** The matrix can ingest third-party results reproducibly;
  no telemetry is added.
- **Validation:** One volunteer run when available. Until then status remains
  `UNVERIFIED` and does not falsely block Samsung-supported completeness.

### Task 4.4: Run lifecycle and resource soaks

- **Location:** `tests/soak/`, diagnostics documentation
- **Description:** Multi-day connect/disconnect, network roam, notification churn,
  clipboard churn, transfers, player churn, phone reboot, host suspend, and daemon
  upgrade. Measure RSS, fds, tasks, stale devices, mounts, subprocesses, and logs.
- **Dependencies:** Sprints 1–2.
- **Acceptance criteria:** Bounded resources, no phantom live state, no unrecovered
  backend, and no repetitive network storm.
- **Validation:** Before/after metrics and automated thresholds.

### Task 4.5: Validate the install path on clean distributions

- **Location:** `packaging/`, `docs/testing.md`, release workflow documentation
- **Description:** Install the published `.deb` (and the binary path) on a clean
  Debian, Ubuntu, and Fedora instance; enable the user service; pair a phone.
  Record first-run findings. The manual walk of 2026-08-05 found three
  first-run defects no test caught (including `226/NAMESPACE` on fresh
  installs); a manual walk is not a gate until it is owned and repeatable.
- **Dependencies:** Sprint 1.
- **Acceptance criteria:** Fresh install reaches a paired, working daemon on each
  distribution, or the failure is a documented, ledgered known issue.
- **Validation:** Recorded run per distribution with artifacts; re-run after each
  release cut.

## Sprint 5: Make evidence closure the release gate

**Goal:** Ensure future features and upstream changes cannot recreate an invisible
gap.

**Demo/validation:** CI rejects an unaccounted capability or unsupported `PASS`, and
the roadmap is generated from the same ledger used by implementation work.

### Task 5.1: Add completeness checks to CI

- **Location:** `.github/workflows/ci.yml`, coverage lint tooling
- **Description:** Gate capability inventory, upstream-fixture schema, evidence-link
  validity, OpenAPI/route parity, and advertised-backend honesty. Keep hardware,
  root, and GUI suites explicitly on-demand with freshness dates rather than fake CI
  coverage.
- **Dependencies:** All prior sprints.
- **Acceptance criteria:** Representative mutations fail the correct gate.
- **Validation:** Mutation-check each guard before merging it.

### Task 5.2: Define release-level completion claims

- **Location:** `ROADMAP.md`, `docs/functional-coverage.md`, release checklist
- **Description:** Use bounded claims: Android-core complete, advertised-feature
  complete, environment-validated, KDE parity, GSConnect parity. Never publish
  "all functional gaps closed" without naming the boundary and evidence date.
  Announcement wording belongs here too: the launch text says "Android-core
  complete on the tested devices; every advertised feature real; the ledger is
  public including its `UNVERIFIED` rows" — never "KDE Connect protocol
  complete", "all environments", or "no bugs". The "built with AI" half of the
  public claim also needs a recorded decision in this task: the public repo
  carries no AI-assist evidence (collapsed history, no trailers), so choose the
  substantiation — personal testimony, a curated craft log, or restored selected
  history — before any announcement, not during.
- **Dependencies:** Task 5.1.
- **Acceptance criteria:** Release notes are mechanically derived from ledger status;
  unknowns and intentional divergences remain visible; the AI-provenance decision
  is recorded and referenced by the announcement draft.
- **Validation:** Dry-run a release summary from the ledger.

### Task 5.3: Add upstream drift review

- **Location:** scheduled CI or documented pre-release command
- **Description:** Compare pinned inventories with upstream heads and open one
  actionable drift report, not one task per changed packet.
- **Dependencies:** Tasks 0.1 and 5.1.
- **Acceptance criteria:** New upstream plugins/capabilities cannot remain invisible;
  no standing automation mutates implementation or claims parity automatically.
- **Validation:** Test against a fixture containing one synthetic upstream addition.

### Task 5.4: Gate public-document truthfulness

- **Location:** `README.md`, `ROADMAP.md`, `SECURITY.md`,
  `KDECONNECT_PROTOCOL.md`, `CONTRIBUTING.md`, CI
- **Description:** One-time scrub of existing drift, then an ongoing check.
  Known drift at audit time: SECURITY.md conflates the 1800 s pair-timestamp
  staleness window with the 30 s/25 s pairing lifetimes; KDECONNECT_PROTOCOL.md
  "Common Bugs" describes long-fixed behavior; the README's TLS-roles sentence
  is true-but-ambiguous under the TCP/TLS role inversion; the share body-limit
  docs say 100 MiB where the router allows 101 MiB; the README lists two fuzz
  targets where three exist. Where a number or capability list can be generated
  from the ledger or the route table, generate it; where it cannot, CI
  cross-checks it.
- **Dependencies:** Task 5.1.
- **Acceptance criteria:** No public claim contradicts current code; capability
  and count claims are either generated or cross-checked in CI.
- **Validation:** Mutation-check: change a plugin count or endpoint in code and
  watch the doc check fail.

## Testing strategy

- Unit tests verify local invariants, never interoperability by themselves.
- Wire tests must use upstream-derived literal fixtures or an independent peer.
- Integration tests must assert the receiving side's effect, not merely a successful
  send or log line.
- Live tests record device/app/OS versions and both sides of the observation.
- Fault tests cover malformed unauthenticated peers, malicious paired peers,
  blackholes, process death, storage exhaustion, and cancellation.
- Environment tests exercise the actual session services used by the backend.
- Every fixed live defect gains the lowest-level deterministic regression test that
  would have failed before the fix.

## Risks and gotchas

- Plugin names do not map one-to-one across implementations; inventory comparison
  must be capability/role based, not directory-name subtraction.
- A backend that logs degradation while continuing to advertise can look robust but
  is functionally dishonest. Capability negotiation must reflect availability.
- KDE-only features may have no Android test path. Keep them classified as parity
  debt rather than manufacturing a core blocker.
- VMs are poor evidence for uinput, audio hardware, multicast roaming, suspend, and
  compositor behavior. Use them for repeatability, then retain targeted hardware
  checks.
- Android restrictions can masquerade as Rust defects, especially clipboard,
  notifications, SMS, and background execution. Observe Android logs/UI before
  assigning cause.
- Upstream changes during the project can move the target. Pin evidence per sprint
  and review drift at boundaries rather than continuously chasing head.
- The API is a control surface, not proof the underlying desktop integration works.
- Announcement prose drifts toward "complete" under pressure — this project's own
  history shows it. The bound in Task 5.2 is only as strong as the hand-written
  launch text being checked against the ledger before posting.
- Ledger cells evidenced by our own logs are self-referential one layer up: an
  effect `PASS` needs a peer-side artifact, exactly like a wire `PASS` needs a
  non-self fixture.

## Rollback plan

- Each backend and protocol change lands independently behind honest capability
  detection; reverting one must not require reverting the coverage ledger.
- Preserve the last known-good release binary and data-directory backup before live
  daemon upgrades.
- New persistent formats require forward migration plus a documented restore path.
- If a backend destabilizes the daemon, withhold only its capabilities and keep the
  protocol core operating; record the row as `FAIL`, never as silently degraded
  `PASS`.
