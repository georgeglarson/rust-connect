# Functional coverage ledger

This ledger is the single source of truth for whether each advertised
capability, behavior, and platform reach is actually working. PASS rows have
citable evidence (peer-side artifacts, environment receipts, or a referenced
test). Everything else carries a `reason` and an `owner` task reference so a
reader can find the follow-up.

Every row claims one of five statuses:

- **PASS** — citable evidence exists. `cite` field names the artifact.
- **FAIL** — broken; `reason` + `owner` say where the fix lives.
- **UNVERIFIED** — not yet tested; `reason` describes what would prove it.
- **NOT-APPLICABLE** — the row's intersection doesn't exist on this surface.
- **INTENTIONAL-DIVERGENCE** — upstream differs on purpose; `reason` records
  the policy, `owner` records where the divergence is documented.

`Slice 0A` (2026-08-05) seeds the ledger with three matrices and the
status-vocabulary schema. A thin schema-lint test refuses to merge unknown
statuses, missing rows for any Rust plugin or upstream-only role, and a
non-PASS row without a reason.

The machine-readable portion lives in fenced YAML blocks immediately under
each matrix heading. The lint parses them. Markdown prose above and below the
fences is human context only and is not parsed.

---

## Feature ledger

One row per feature/role. Rows come from three pools:

- All 24 production plugins (seeded from
  `tests/fixtures/rust-capabilities.yaml`).
- The behavioral rows of `docs/parity-checklist.md` (Discovery, Link layer,
  Pairing, Packet handling, Payload transfers, Lifecycle).
- Every upstream-only role from
  `tests/fixtures/upstream-capabilities/{kdeconnect-kde,gsconnect,kdeconnect-android}.yaml`
  (seeded UNVERIFIED, owner = Sprint 3 / Task 3.1).

`rust_impl` is `true` when the row corresponds to a plugin/module under
`src/plugins/`. Upstream-only rows use `rust_impl: false` and
`upstream: kdeconnect-kde|gsconnect|kdeconnect-android`.

Eight evidence dimensions per the plan:

- `upstream_ref` — kde/android/gsconnect file:line backing this row.
- `desktop_effect` — what a real desktop session observes. PASS requires
  a peer-side artifact, not just our log line.
- `api_surface` — REST endpoint(s) and CLI flag(s) the feature exposes.
- `lifecycle` — connect / disconnect / unpair / pair-completion behavior.
- `hostile_input` — malformed-input / authorization behaviors.
- `fixture_provenance` — wire-conformance test source: upstream-derived
  literal, independent peer, or Rust-self (the last is the defect class
  Task 0.4 converts away).
- `live_device` — A15 / S21 / other-Android observation.
- `environment` — which desktop backend (X11/Wayland, audio, session D-Bus,
  notification server) the row is verified on.

`cite` is the citation token for the row. For PASS, it must point to a
citable artifact (file:line of an upstream-derived fixture, peer-side
log/screenshot, or a documented `docs/live-validation.md` entry).

```yaml
feature_ledger:
  - feature: battery
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/battery/ / kdeconnect-android src/main/java/.../BatteryPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: PASS
    cite: "docs/live-validation.md 2026-08-02 Battery row (live 90%, charging); docs/parity-checklist.md Discovery/Lifecycle CONFORMANT"
    reason:
    owner:

  - feature: clipboard
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/clipboard/ / kdeconnect-android .../ClipboardPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: PASS
    cite: "docs/live-validation.md 2026-08-02 Clipboard desktop<->phone rows"
    reason:
    owner:

  - feature: connectivity
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/connectivity-report/ / kdeconnect-android ConnectivityReportPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: PASS
    environment: UNVERIFIED
    status: PASS
    cite: "docs/live-validation.md 2026-08-02 Connectivity row"
    reason:
    owner:

  - feature: contacts
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/contacts/ / kdeconnect-android ContactsPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: digitizer
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/digitizer/ / kdeconnect-android DigitizerPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: findmyphone
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/findmyphone/ / kdeconnect-android FindMyPhonePlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: findthisdevice
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/findthisdevice/ (no android equivalent — desktop-origin)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 1 / Task 1.6 verification"
    owner: "Task 1.6"

  - feature: lock
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/lockdevice/ (no android equivalent — desktop-origin)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 1 / Task 1.6 verification"
    owner: "Task 1.6"

  - feature: mousepad
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/mousepad/ / kdeconnect-android MousePadPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 1 / Task 1.6 absolute-axes verification"
    owner: "Task 1.6"

  - feature: mpris
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/mprisremote/ + mpriscontrol/ / kdeconnect-android MprisReceiverPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "Sprint 1 / Task 1.5 album-art and session-bus work pending"
    owner: "Task 1.5"

  - feature: notification
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/notifications/ / kdeconnect-android NotificationsPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: PASS
    environment: UNVERIFIED
    status: PASS
    cite: "docs/live-validation.md 2026-08-02 Notification desktop->phone and mirror rows"
    reason:
    owner:

  - feature: pausemusic
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/pausemusic/ (no android equivalent)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "Sprint 1 / Task 1.6 mute-vs-pause policy pending"
    owner: "Task 1.6"

  - feature: ping
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/ping/ / kdeconnect-android PingPlugin.kt"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: PASS
    cite: "docs/live-validation.md 2026-08-02 Ping row"
    reason:
    owner:

  - feature: presenter
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/presenter/ / kdeconnect-android PresenterPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: remotecommands
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/remotecommands/ (no android equivalent)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 1 / Task 1.2 authorization model pending"
    owner: "Task 1.2"

  - feature: remotekeyboard
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/remotekeyboard/ / kdeconnect-android RemoteKeyboardPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: runcommand
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/runcommand/ / kdeconnect-android RunCommandPlugin.java"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "Sprint 1 / Task 1.2 allowlist + output-stream work pending"
    owner: "Task 1.2"

  - feature: screensaver-inhibit
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/screensaver-inhibit/ (no android equivalent)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: sendnotifications
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sendnotifications/ (no android equivalent — phone-originated)"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "Sprint 1 / Task 1.4 inline-action + reply/dismiss pending"
    owner: "Task 1.4"

  - feature: sftp
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sftp/ / kdeconnect-android SftpPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "Sprint 1 / Task 1.3 mount + credential-cleanup pending"
    owner: "Task 1.3"

  - feature: share
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/share/ / kdeconnect-android SharePlugin.java"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: PASS
    cite: "docs/live-validation.md 2026-08-02 Share desktop<->phone rows + 81 KiB PNG receipt"
    reason:
    owner:

  - feature: sms
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/sms/ / kdeconnect-android SMSPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: systemvolume
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/systemvolume/ + remotesystemvolume/ / kdeconnect-android SystemVolumePlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: PASS
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite: "src/plugins/systemvolume/ + functional-coverage REST routes /api/v1/systemvolume/sinks{,:name/control}; backend契约覆盖 pactl JSON、subscribe events"
    reason: "Provider implemented (pactl + subscribe supervision + REST surface); live_device + environment + lifecycle evidence remain the integrator's job"
    owner: "Task 1.1"

  - feature: telephony
    rust_impl: true
    upstream:
    upstream_ref: "kdeconnect-kde plugins/telephony/ / kdeconnect-android TelephonyPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  # Behavioral parity rows — sourced from docs/parity-checklist.md.
  # A PASS row carries a cite to a docs/ artifact; failure sources stay
  # explicit (see Gaps section in parity-checklist.md).
  - feature: discovery-broadcast-cadence
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:149,192 / LanLinkProvider.java:567,573-577"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: UNVERIFIED
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "docs/parity-checklist.md Discovery broadcast cadence row"
    reason: "deliberate pre-mDNS periodic broadcast; revisit after mDNS live validation"
    owner: "Sprint 0 / Task 2.2"

  - feature: discovery-network-change-rebroadcast
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:180-194 / LanLinkProvider.java:572-584"
    desktop_effect: FAIL
    api_surface: FAIL
    lifecycle: FAIL
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: FAIL
    cite: "docs/parity-checklist.md Gaps #5"
    reason: "no network-change hook"
    owner: "Sprint 2 / Task 2.2"

  - feature: udp-receive-buffer
    rust_impl: true
    upstream: kdeconnect-android
    upstream_ref: "LanLinkProvider.java:69"
    desktop_effect: FAIL
    api_surface: FAIL
    lifecycle: FAIL
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "docs/parity-checklist.md Gaps #4"
    reason: "64 KiB instead of android 512 KiB; oversized identity truncates and drops. Need vk-backed decision."
    owner: "Sprint 2 / Task 2.1"

  - feature: payload-accept-timeout
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "compositeuploadjob.cpp:35-37"
    desktop_effect: FAIL
    api_surface: FAIL
    lifecycle: FAIL
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "docs/parity-checklist.md Gaps #2"
    reason: "300 s vs 30 s (kde) / 10 s (android). Over-lenient; tracked for fix."
    owner: "Sprint 2 / Task 2.1"

  - feature: tls-role-inversion
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:391,573"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: PASS
    cite: "docs/parity-checklist.md Link layer TLS-role row CONFORMANT"
    reason:
    owner:

  - feature: pairing-sas-displayed
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "pairinghandler.cpp:176-195 / PairingHandler.kt:239-255"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: PASS
    environment: UNVERIFIED
    status: PASS
    cite: "docs/live-validation.md 2026-08-05 'Phone-initiated pairing: SAS verified identical on both devices' (key 65D58104)"
    reason:
    owner:

  - feature: cad-pair-false-on-unpaired-traffic
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "core/device.cpp:391-394"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: PASS
    cite: "docs/parity-checklist.md Link layer 'Unpaired device sends non-pair packet' row CONFORMANT (fixed 2026-08-04)"
    reason:
    owner:

  - feature: identity-tls-exchange-with-rejection
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "lanlinkprovider.cpp:434-445"
    desktop_effect: PASS
    api_surface: PASS
    lifecycle: PASS
    hostile_input: PASS
    fixture_provenance: PASS
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: PASS
    cite: "docs/parity-checklist.md Link layer 'v8 encrypted identity re-exchange' row CONFORMANT"
    reason:
    owner:

  - feature: cap-overwrite-on-empty-identity
    rust_impl: true
    upstream: kdeconnect-kde
    upstream_ref: "core/device.cpp:319-328"
    desktop_effect: FAIL
    api_surface: FAIL
    lifecycle: FAIL
    hostile_input: PASS
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: INTENTIONAL-DIVERGENCE
    cite: "docs/parity-checklist.md Gaps #3"
    reason: "rust upsert overwrites unconditionally; kde applies only when both lists non-empty. Real peers always send caps today, so the divergence is not currently reachable from production."
    owner: "Sprint 2 / Task 2.1"

  # Upstream-only roles seeded UNVERIFIED. Each role appears under exactly
  # one implementation; Sprint 3 / Task 3.1 decides which map to Rust code,
  # which become intentional divergences, and which become out-of-scope.

  - feature: kdeconnect-kde/connectivity_report
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/connectivity-report/kdeconnect_connectivity_report.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `connectivity` (see that row)"
    reason: "rolled-up to rust plugin `connectivity`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/lockdevice
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/lockdevice/kdeconnect_lockdevice.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `lock` (see that row)"
    reason: "rolled-up to rust plugin `lock`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/mmtelephony
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mmtelephony/kdeconnect_mmtelephony.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/mpriscontrol
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mpriscontrol/kdeconnect_mpriscontrol.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment (rust plugin `mpris` covers KDE split into remote+control)"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/mprisremote
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mprisremote/kdeconnect_mprisremote.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment (rust plugin `mpris` covers KDE split into remote+control)"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/notifications
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/notifications/kdeconnect_notifications.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `notification`"
    reason: "rolled-up to rust plugin `notification`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/pausemusic
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/pausemusic/kdeconnect_pausemusic.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `pausemusic`"
    reason: "rolled-up to rust plugin `pausemusic`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/ping
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/ping/kdeconnect_ping.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `ping`"
    reason: "rolled-up to rust plugin `ping`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/presenter
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/presenter/kdeconnect_presenter.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `presenter`"
    reason: "rolled-up to rust plugin `presenter`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/remotecommands
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/remotecommands/kdeconnect_remotecommands.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `remotecommands`"
    reason: "rolled-up to rust plugin `remotecommands`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/remotecontrol
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/remotecontrol/kdeconnect_remotecontrol.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/remotekeyboard
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/remotekeyboard/kdeconnect_remotekeyboard.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `remotekeyboard`"
    reason: "rolled-up to rust plugin `remotekeyboard`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/remotesystemvolume
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/remotesystemvolume/kdeconnect_remotesystemvolume.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment (controller side of systemvolume)"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/runcommand
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/runcommand/kdeconnect_runcommand.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `runcommand`"
    reason: "rolled-up to rust plugin `runcommand`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/screensaver-inhibit
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/screensaver-inhibit/kdeconnect_screensaver_inhibit.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `screensaver-inhibit`"
    reason: "rolled-up to rust plugin `screensaver-inhibit`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/sendnotifications
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/sendnotifications/kdeconnect_sendnotifications.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sendnotifications`"
    reason: "rolled-up to rust plugin `sendnotifications`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/sftp
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/sftp/kdeconnect_sftp.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sftp`"
    reason: "rolled-up to rust plugin `sftp`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/share
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/share/kdeconnect_share.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `share`"
    reason: "rolled-up to rust plugin `share`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/shareinputdevices
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/shareinputdevices/kdeconnect_shareinputdevices.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/shareinputdevicesremote
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/shareinputdevicesremote/kdeconnect_shareinputdevicesremote.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/sms
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/sms/kdeconnect_sms.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sms`"
    reason: "rolled-up to rust plugin `sms`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/systemvolume
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/systemvolume/kdeconnect_systemvolume.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 1 / Task 1.1 audio backend pending"
    owner: "Task 1.1"

  - feature: kdeconnect-kde/telephony
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/telephony/kdeconnect_telephony.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `telephony`"
    reason: "rolled-up to rust plugin `telephony`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/virtualmonitor
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/virtualmonitor/kdeconnect_virtualmonitor.json"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: NOT-APPLICABLE
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  # Android-only roles not yet mapped to a Rust plugin.
  - feature: kdeconnect-android/inputdevicesreceiver
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../plugins/inputdevicesreceiver/"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "no Plugin.java/PACKET_TYPE declarations in this android directory; upstream SKU — Sprint 3 / Task 3.1 alignment"
    owner: "Task 3.1"

  - feature: kdeconnect-android/mousereceiver
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../MouseReceiverPlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "unclassified — Sprint 3 / Task 3.1 alignment (android-only — Rust plugin `mousepad` is the receive side for both)"
    owner: "Task 3.1"

  - feature: kdeconnect-android/findremotedevice
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../FindRemoteDevicePlugin.kt"
    desktop_effect: UNVERIFIED
    api_surface: UNVERIFIED
    lifecycle: UNVERIFIED
    hostile_input: UNVERIFIED
    fixture_provenance: UNVERIFIED
    live_device: UNVERIFIED
    environment: UNVERIFIED
    status: UNVERIFIED
    cite:
    reason: "android-only — findremotedevice's outgoing packet type is FindMyPhonePlugin.PACKET_TYPE_FINDMYPHONE_REQUEST (Rust covers via `findmyphone`)"
    owner: "Task 3.1"

  # GSConnect-only roles not mapped to a Rust plugin.
  - feature: gsconnect/connectivity_report
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/connectivity_report.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `connectivity`"
    reason: "rolled-up to rust plugin `connectivity`"
    owner: "Task 3.1"

  - feature: gsconnect/notification
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/notification.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `notification`"
    reason: "rolled-up to rust plugin `notification`"
    owner: "Task 3.1"

  # GSConnect-only role rows that map 1:1 to a Rust plugin. Each is recorded
  # as NOT-APPLICABLE with the cite pointing at the Rust plugin's row above.
  - feature: gsconnect/battery
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/battery.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `battery`"
    reason: "rolled-up to rust plugin `battery`"
    owner: "Task 3.1"

  - feature: gsconnect/clipboard
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/clipboard.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `clipboard`"
    reason: "rolled-up to rust plugin `clipboard`"
    owner: "Task 3.1"

  - feature: gsconnect/contacts
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/contacts.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `contacts`"
    reason: "rolled-up to rust plugin `contacts`"
    owner: "Task 3.1"

  - feature: gsconnect/findmyphone
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/findmyphone.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `findmyphone`"
    reason: "rolled-up to rust plugin `findmyphone`"
    owner: "Task 3.1"

  - feature: gsconnect/mousepad
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/mousepad.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mousepad`"
    reason: "rolled-up to rust plugin `mousepad`"
    owner: "Task 3.1"

  - feature: gsconnect/mpris
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/mpris.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mpris`"
    reason: "rolled-up to rust plugin `mpris`"
    owner: "Task 3.1"

  - feature: gsconnect/ping
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/ping.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `ping`"
    reason: "rolled-up to rust plugin `ping`"
    owner: "Task 3.1"

  - feature: gsconnect/presenter
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/presenter.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `presenter`"
    reason: "rolled-up to rust plugin `presenter`"
    owner: "Task 3.1"

  - feature: gsconnect/runcommand
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/runcommand.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `runcommand`"
    reason: "rolled-up to rust plugin `runcommand`"
    owner: "Task 3.1"

  - feature: gsconnect/sftp
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/sftp.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sftp`"
    reason: "rolled-up to rust plugin `sftp`"
    owner: "Task 3.1"

  - feature: gsconnect/share
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/share.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `share`"
    reason: "rolled-up to rust plugin `share`"
    owner: "Task 3.1"

  - feature: gsconnect/sms
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/sms.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sms`"
    reason: "rolled-up to rust plugin `sms`"
    owner: "Task 3.1"

  - feature: gsconnect/systemvolume
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/systemvolume.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `systemvolume`"
    reason: "rolled-up to rust plugin `systemvolume`"
    owner: "Task 3.1"

  - feature: gsconnect/telephony
    rust_impl: false
    upstream: gsconnect
    upstream_ref: "GSConnect src/service/plugins/telephony.js"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `telephony`"
    reason: "rolled-up to rust plugin `telephony`"
    owner: "Task 3.1"

  # kdeconnect-android role rows that map 1:1 to a Rust plugin.
  - feature: kdeconnect-android/battery
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../battery/BatteryPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `battery`"
    reason: "rolled-up to rust plugin `battery`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/clipboard
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../clipboard/ClipboardPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `clipboard`"
    reason: "rolled-up to rust plugin `clipboard`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/connectivityreport
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../connectivityreport/ConnectivityReportPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `connectivity`"
    reason: "rolled-up to rust plugin `connectivity`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/contacts
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../contacts/ContactsPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `contacts`"
    reason: "rolled-up to rust plugin `contacts`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/digitizer
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../digitizer/DigitizerPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `digitizer`"
    reason: "rolled-up to rust plugin `digitizer`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/findmyphone
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../findmyphone/FindMyPhonePlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `findmyphone`"
    reason: "rolled-up to rust plugin `findmyphone`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/mousepad
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../mousepad/MousePadPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mousepad`"
    reason: "rolled-up to rust plugin `mousepad`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/mpris
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../mpris/MprisPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mpris`"
    reason: "rolled-up to rust plugin `mpris`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/mprisreceiver
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../mprisreceiver/MprisReceiverPlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mpris` (receive side)"
    reason: "rolled-up to rust plugin `mpris`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/notifications
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../notifications/NotificationsPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `notification`"
    reason: "rolled-up to rust plugin `notification`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/ping
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../ping/PingPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `ping`"
    reason: "rolled-up to rust plugin `ping`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/presenter
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../presenter/PresenterPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `presenter`"
    reason: "rolled-up to rust plugin `presenter`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/receivenotifications
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../receivenotifications/ReceiveNotificationsPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sendnotifications`"
    reason: "rolled-up to rust plugin `sendnotifications` (mirror receive side)"
    owner: "Task 3.1"

  - feature: kdeconnect-android/remotekeyboard
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../remotekeyboard/RemoteKeyboardPlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `remotekeyboard`"
    reason: "rolled-up to rust plugin `remotekeyboard`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/runcommand
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../runcommand/RunCommandPlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `runcommand`"
    reason: "rolled-up to rust plugin `runcommand`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/sftp
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../sftp/SftpPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sftp`"
    reason: "rolled-up to rust plugin `sftp`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/share
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../share/SharePlugin.java"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `share`"
    reason: "rolled-up to rust plugin `share`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/sms
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../sms/SMSPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `sms`"
    reason: "rolled-up to rust plugin `sms`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/systemvolume
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../systemvolume/SystemVolumePlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `systemvolume`"
    reason: "rolled-up to rust plugin `systemvolume`"
    owner: "Task 3.1"

  - feature: kdeconnect-android/telephony
    rust_impl: false
    upstream: kdeconnect-android
    upstream_ref: "kdeconnect-android src/main/java/.../telephony/TelephonyPlugin.kt"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `telephony`"
    reason: "rolled-up to rust plugin `telephony`"
    owner: "Task 3.1"

  # kdeconnect-kde role rows that map 1:1 to a Rust plugin.
  - feature: kdeconnect-kde/battery
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/battery/kdeconnect_battery.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `battery`"
    reason: "rolled-up to rust plugin `battery`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/clipboard
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/clipboard/kdeconnect_clipboard.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `clipboard`"
    reason: "rolled-up to rust plugin `clipboard`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/contacts
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/contacts/kdeconnect_contacts.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `contacts`"
    reason: "rolled-up to rust plugin `contacts`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/digitizer
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/digitizer/kdeconnect_digitizer.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `digitizer`"
    reason: "rolled-up to rust plugin `digitizer`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/findmyphone
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/findmyphone/kdeconnect_findmyphone.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `findmyphone`"
    reason: "rolled-up to rust plugin `findmyphone`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/findthisdevice
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/findthisdevice/kdeconnect_findthisdevice.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `findthisdevice`"
    reason: "rolled-up to rust plugin `findthisdevice`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/mousepad
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/mousepad/kdeconnect_mousepad.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `mousepad`"
    reason: "rolled-up to rust plugin `mousepad`"
    owner: "Task 3.1"

  - feature: kdeconnect-kde/screensaver_inhibit
    rust_impl: false
    upstream: kdeconnect-kde
    upstream_ref: "kdeconnect-kde plugins/screensaver-inhibit/kdeconnect_screensaver_inhibit.json"
    desktop_effect: NOT-APPLICABLE
    api_surface: NOT-APPLICABLE
    lifecycle: NOT-APPLICABLE
    hostile_input: NOT-APPLICABLE
    fixture_provenance: NOT-APPLICABLE
    live_device: NOT-APPLICABLE
    environment: NOT-APPLICABLE
    status: NOT-APPLICABLE
    cite: "row satisfied as the rust plugin `screensaver-inhibit`"
    reason: "rolled-up to rust plugin `screensaver-inhibit`"
    owner: "Task 3.1"
```

---

## Environment matrix

Backends that vary across desktop environments. A feature passes on a backend
when its `desktop_effect` evidence came from that specific backend on a
real session, not when an upstream-spec source implies it.

```yaml
environment_matrix:
  # Keyed by feature. Each value lists the per-backend status.
  - feature: clipboard-write
    rust_impl: true
    clipboard-x11: UNVERIFIED
    clipboard-wayland: UNVERIFIED
    uinput: NOT-APPLICABLE
    audio: NOT-APPLICABLE
    session_dbus: PASS
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite:
    reason: "wayland portal design depends on compositor; Task 1.6 X11 backend pending"
    owner: "Task 1.6"

  - feature: mousepad-absolute
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: UNVERIFIED
    uinput: UNVERIFIED
    audio: NOT-APPLICABLE
    session_dbus: NOT-APPLICABLE
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite:
    reason: "absolute-axis support is a known gap; Task 1.6 verification on uinput pending"
    owner: "Task 1.6"

  - feature: mpris-control
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: NOT-APPLICABLE
    audio: PASS
    session_dbus: PASS
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite:
    reason: "D-Bus path verified manually (tests/mpris_session_bus.rs), but real-media-player verification pending; Task 1.5"
    owner: "Task 1.5"

  - feature: notification-mirror
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: NOT-APPLICABLE
    audio: NOT-APPLICABLE
    session_dbus: PASS
    notification_server: PASS
    status: PASS
    cite: "docs/live-validation.md 2026-08-02 Notification mirror row (Digital Wellbeing mirrored)"
    reason:
    owner:

  - feature: systemvolume-provider
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: NOT-APPLICABLE
    audio: UNVERIFIED
    session_dbus: NOT-APPLICABLE
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite: "src/plugins/systemvolume/backend.rs::PactlBackend (pactl list/subscribe/set), fixture-derived wire assertions"
    reason: "pactl backend implemented + mock-tested; live PipeWire / PulseAudio session verification remains the integrator's job"
    owner: "Task 1.1"

  - feature: inputdevices-uinput
    rust_impl: true
    clipboard-x11: NOT-APPLICABLE
    clipboard-wayland: NOT-APPLICABLE
    uinput: UNVERIFIED
    audio: NOT-APPLICABLE
    session_dbus: NOT-APPLICABLE
    notification_server: NOT-APPLICABLE
    status: UNVERIFIED
    cite:
    reason: "uinput backend reach not environment-validated yet"
    owner: "Sprint 4 / Task 4.1"
```

---

## Device matrix

Per-feature device reach. `A15` and `S21` are the two test handsets. Other
Android is the volunteer-derived slot.

```yaml
device_matrix:
  - feature: ping
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "S21 verification cited under feature ledger"
    reason: "A15 not yet exercised for this feature"
    owner: "Sprint 4 / Task 4.2"

  - feature: battery
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "S21 verification cited under feature ledger"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: clipboard-desktop-to-phone
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Clipboard desktop->phone row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: clipboard-phone-to-desktop
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Clipboard phone->desktop row (Android 10+ foreground caveat)"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: share-desktop-to-phone
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Share desktop->phone row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: share-phone-to-desktop
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Share phone->desktop row (81 KiB PNG receipt)"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: notification-desktop-to-phone
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Notification desktop->phone row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: notification-mirror-phone-to-desktop
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Notification mirror row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: unpair-both-severed
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-02 Unpair row"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"

  - feature: fresh-pair-sas-matched
    A15: UNVERIFIED
    S21: PASS
    other_android: UNVERIFIED
    status: UNVERIFIED
    cite: "docs/live-validation.md 2026-08-05 'Phone-initiated pairing: SAS verified identical on both devices'"
    reason: "A15 not yet exercised"
    owner: "Sprint 4 / Task 4.2"
```

---

## Evidence ledger schema (intentional divergences and gaps still open)

Carried forward from `docs/parity-checklist.md` Gaps section, with the
ledger row that resolves it:

| Gap | Source row | Tracker |
|---|---|---|
| Broadcast-forever cadence | feature_ledger discovery-broadcast-cadence | Task 2.2 |
| Capability overwrite on empty identity | feature_ledger cap-overwrite-on-empty-identity | Task 2.1 |
| UDP receive buffer 64 KiB | feature_ledger udp-receive-buffer | Task 2.1 |
| Payload accept timeout 300 s | feature_ledger payload-accept-timeout | Task 2.1 |
| Network-change re-broadcast trigger | feature_ledger discovery-network-change-rebroadcast | Task 2.2 |

Any new intentional divergence added to the ledger must carry a `reason`
and an `owner` task reference per the schema-lint test.

