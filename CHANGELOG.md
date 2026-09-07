# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- Cutting a release: rename the [Unreleased] heading below to
     [X.Y.Z] - YYYY-MM-DD, add a fresh empty [Unreleased] section above it,
     and update the two link definitions at the bottom of this file. -->

## [Unreleased]

### Added

- mDNS storm sensor: the browse loop samples `ServiceDaemon::get_metrics`
  every 60 s, sums the outbound-packet-send counters (`register-resend`,
  `unregister-resend`, `respond`, and the three `cache-refresh-*` packets),
  and logs `event = "mdns_send_storm"` at WARN with the moved deltas when
  sends per minute cross `MDNS_STORM_THRESHOLD_PER_MIN` (600/min ≈ 10/s,
  ~12× the steady-state rate). A failed `get_metrics` is logged once at
  DEBUG and skipped — the sensor never ends the browse loop.
- `GET /api/v1/events` sends a `: keepalive` comment every 15 seconds so
  a dead upstream surfaces as a closed connection within ~15s rather
  than a half-open socket the client believes is still live. Cadence
  sits below typical reverse-proxy idle timeouts.
- `GET /api/v1/events` includes a `kind` discriminator on every event
  frame, in `<enum>.<variant>` form (`device.discovered`,
  `device.state_changed`, `plugin.notification`, …). Existing
  `event_type` / `type` fields are preserved unchanged so existing
  consumers keep working; the `kind` key namespaces the two source
  enums on a single shared vocabulary.
- `GET /api/v1/events` carries a process-global monotonic `id: <n>`
  line on every event and lagged frame, shared across the device and
  plugin streams. Honoring `Last-Event-ID` is out of scope today;
  ids are observation-only.
- `GET /api/v1/events` ships a named `event: snapshot` frame as the
  first frame on every (re)connect, carrying the same JSON
  `GET /api/v1/devices` returns in its `data` envelope (full
  `pair_state` + `verification_key` overlay applied). Lets a fresh
  subscriber render the device pane on connect and after a `lagged`
  frame without a follow-up REST call.
- The binary knows its build: `rust-connect --version` prints
  `<version> (<git sha>[-dirty])` and `GET /api/v1/health` carries a
  `build` object with `version`, `git_sha`, and `dirty`, so an installed
  daemon can be compared to `origin/main` (vk #973).
- Split-brain detector: an identity announcing from one of this host's own
  addresses under a foreign device id logs `split_brain_suspected` (three
  such incidents in a month competed for the paired phones), and a port
  1716 bind failure names the process holding the port instead of
  guessing.

- The systemd journal is now the log sink under systemd, as structured
  records: every log line's `event` field is a journal field, so
  `journalctl --user -u rust-connect EVENT=split_brain_suspected` works
  and the journal no longer holds two lines per event with terminal
  colour codes. On a terminal the output is text; `LOG_FORMAT=json` still
  selects JSON lines.
- `GET /api/v1/devices` documents its `page` and `limit` query parameters
  (it honoured them before; the spec did not say so).

### Changed

- The `X-Request-ID` response header and the envelope's
  `metadata.request_id` are now the same id, and it is the id the
  `api_request` / `api_response` log lines carry. They were three
  independent UUIDs.
- The per-IP API rate limiter is off when the API binds a loopback
  address (the default): every local client was 127.0.0.1 and shared one
  100-requests-per-minute bucket. A non-loopback bind keeps the limiter,
  and its counter update is now atomic.
- The systemd unit no longer sets `RUST_LOG`; `log_level` in config.toml
  is the source of truth and `RUST_LOG` (in a drop-in) is the override.
- Dropped an unused `config` crate (a 54-crate subtree) and three unused
  dev-dependencies; 273 → 250 crates in a production build.
- The tool catalogue at `GET /api/v1/tools` moved from a hand-written
  match in the API layer onto the `Plugin` trait itself. Each plugin
  that owns a REST route now declares its `Tool` entries from
  `tools()`; the API layer walks the registry, applies
  `is_backend_available`, and dedupes+sorts. Wire shape (JSON, OpenAPI
  schema) is byte-identical. Pinned by `tests/route_table_lint.rs`
  (nine-name pin, default-empty pin, and a device-route ratchet).

### Fixed

- `cargo test` on the same host as a running daemon no longer reaches it:
  a failing dial inside the suite used to unicast the fixture identity to
  `<peer>:1716` as the reverse-connection fallback, which on the daemon
  host is the live daemon, and test builds could bind the production port
  alongside it. Test builds now use a port outside the KDE Connect range
  for both, and refuse the production port outright.
- A split-brain identity (a foreign device id announcing from one of this
  host's own addresses) is now refused, not just logged: no registry
  record, no dial. Previously the daemon warned and then registered and
  dialed the other daemon anyway.
- The deb's postinst no longer runs `systemctl --global enable`, which
  started a daemon in every user manager on the host, greeter users
  included (Fedora's `gdm-greeter` ran its own instance at every boot with
  a fresh identity, dialed paired phones, and held port 1716 until login).
  Upgrading now removes a prior global enable; enabling is per user with
  `systemctl --user enable --now rust-connect.service`.
  Pinned by `tests/packaging_lint.rs`.
- `devices.json` was written as `{}` on every save: the device registry
  borrowed its paired-ids handle from a pairing handler that was then
  discarded, so no device ever counted as paired for persistence or for the
  unpaired-eviction cap. Device records now survive a restart.
- SFTP mounts target the address of the authenticated link, never the `ip`
  the packet claims, and the `user`/`path` fields are validated before they
  become sshfs arguments (a `user` beginning `-oProxyCommand=` was command
  execution from a paired peer).
- A send that fails or times out now tears the link down instead of leaving
  a half-written packet queued ahead of the next one, which the peer then
  dropped along with it.
- `cargo test` no longer announces fixture identities to real KDE Connect
  peers on the LAN: test builds use a test-only mDNS service type.

First public release. Everything below is the initial feature set rather
than a delta against a previous version.

### Added

- KDE Connect protocol on the LAN: UDP discovery, TCP transport, TLS 1.2
  with mutual authentication and TOFU certificate pinning, and SAS pairing
  byte-compatible with the Android app's `PairingHandler`.
- 25 plugins: ping, battery, notification, sms, clipboard, share, mpris,
  telephony, pausemusic, connectivity, sftp, mousepad, lock, systemvolume,
  findmyphone, findthisdevice, presenter, contacts, runcommand,
  sendnotifications, remotekeyboard, digitizer, screensaver-inhibit,
  remotecommands, and shareinputdevices.
- Real desktop integration for clipboard (wl-clipboard, both directions
  with echo suppression) and MPRIS (zbus on the session bus, player
  discovery plus play/pause/seek/volume relay). Both degrade to a logged
  no-op rather than failing when the session is unavailable.
- REST API at `/api/v1/` with an OpenAPI spec at `/api-docs/openapi.json`
  and Swagger UI at `/docs`.
- SSE event stream at `/api/v1/events`, carrying device and plugin events
  on one connection.
- Embedded troubleshooting web UI served from the binary at `/ui`.
- CLI client mode: `status`, `devices`, `pair`, `unpair`, `ping`, `share`,
  and `clipboard` drive a running daemon over its REST API, with `--json`
  output, `--api-url` / `--api-key` (and `RUST_CONNECT_API_URL` /
  `RUST_CONNECT_API_KEY`) overrides, device-id prefix matching, and
  distinct exit codes for API errors (1) and an unreachable daemon (2).
- Multipart file upload on the share endpoint, streamed rather than
  buffered.
- cargo-fuzz targets over `PacketSerializer::deserialize` and the UDP
  identity decode path, with a seeded corpus covering valid packets and
  boundary cases, a 60-second CI smoke pass per target on protocol PRs,
  and a weekly ten-minute run.
- systemd **user** unit plus an installer script, and a Debian package
  build. The unit is deliberately a user unit: identity, pairing state,
  desktop notifications, and downloads all live inside the session.

### Changed

- Trust-core rewrite. Pairing semantics now match the Android app rather
  than approximating it: peer certificates are verified before any write,
  identities are cross-checked and target fields honored, self-connections
  are refused on both dial paths, expired pending requests are treated as
  not-paired, the 1800-second staleness gate applies only to `pair: true`,
  and received device names are sanitized the way Android sanitizes them.
- openssl removed from production dependencies. Certificate handling uses
  rcgen, x509-parser, and sha2; openssl remains a dev-dependency for test
  fixtures only.
- CLI migrated to clap.
- rustls pinned to TLS 1.2 with the `tls12` feature explicitly enabled.
  It is not a default feature under `default-features = false`, and KDE
  Connect requires TLS 1.2, so omitting it is a silent interop break.

### Fixed

- Pairing: plugin init packets are sent on every pairing-completion path;
  stale self-keyed paired entries are pruned; the SAS is available on
  daemon-initiated pairing because the peer certificate is staged first.
- Connections: same-certificate duplicate inbound connections are
  deduplicated against a healthy existing link instead of replacing it.
- Notifications: the reply handle is captured from `requestReplyId`.
- SMS: conversations are requested with the request packet.
- Contacts: vCard 2.1 group prefixes are stripped when parsing fields.
- runcommand: timeouts kill the whole process group, and streamed output
  is capped.
- Device records and notification history report consistent state;
  capabilities are read on the inbound path and `paired_at` comes from the
  pairing record.

### Security

- Hardening pass covering the REST API bind default, API key file
  permissions, and share-transfer size caps.
- The runcommand allowlist ships empty: the advertised command list is
  `{}` and every request is refused until an operator configures one.
- `cargo audit` and `cargo deny` run in CI, weekly as well as per-PR, so
  new advisories surface between commits.

[Unreleased]: https://github.com/georgeglarson/rust-connect/commits/main
