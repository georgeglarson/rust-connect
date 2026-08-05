# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- Cutting a release: rename the [Unreleased] heading below to
     [X.Y.Z] - YYYY-MM-DD, add a fresh empty [Unreleased] section above it,
     and update the two link definitions at the bottom of this file. -->

## [Unreleased]

First public release. Everything below is the initial feature set rather
than a delta against a previous version.

### Added

- KDE Connect protocol on the LAN: UDP discovery, TCP transport, TLS 1.2
  with mutual authentication and TOFU certificate pinning, and SAS pairing
  byte-compatible with the Android app's `PairingHandler`.
- 24 plugins: ping, battery, notification, sms, clipboard, share, mpris,
  telephony, pausemusic, connectivity, sftp, mousepad, lock, systemvolume,
  findmyphone, findthisdevice, presenter, contacts, runcommand,
  sendnotifications, remotekeyboard, digitizer, screensaver-inhibit, and
  remotecommands.
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
