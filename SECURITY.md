# Security Policy

## Reporting a Vulnerability

Report vulnerabilities through GitHub's private vulnerability reporting:
open the repository's **Security** tab and use **Report a vulnerability**.
Do not open a public issue for a security problem.

No PGP key is currently published. If GitHub private reporting is
unavailable to you, say so in a public issue *without* vulnerability
details and a contact channel will be arranged.

### What to include

- Affected commit or version, and how the daemon was built/configured.
- Steps to reproduce, including packet captures or API requests where
  applicable.
- Impact assessment: what an attacker gains, from what position (LAN,
  paired device, local process).
- Whether the behavior diverges from upstream KDE Connect or is inherent
  to the protocol.

### Response expectations

This is a best-effort, solo-maintainer project. There is no SLA.
Expect acknowledgment within a few days and a fix or a documented
decision when time permits. Critical issues (authentication bypass,
remote code execution) take priority.

### Scope

In scope:

- The daemon (`rust-connect` binary) and its plugin system.
- The REST API and the SSE event stream.
- The KDE Connect protocol implementation: discovery, TLS transport,
  pairing, packet handling, payload transfers.

Out of scope: upstream KDE Connect protocol design decisions (see
below), the Android app, and issues requiring physical access.

## Known accepted risks (by design)

The following are deliberate, upstream-compatible trade-offs. They are
documented here so reviewers do not file them as bugs. See
`docs/threat-model.md` for the full analysis.

- **TLS 1.2 pinned in both roles.** The Android client (Conscrypt via
  `SslHelper.kt`) negotiates TLS 1.2; TLS 1.3 is intentionally refused
  for interop. See `src/protocol/connection/tls.rs`.
- **Pre-pairing TOFU accepts any self-signed certificate.** Trust is
  established out-of-band via a 32-bit Short Authentication String (SAS)
  shown on both the initiating and accepting side (CLI, API, web UI, and the daemon journal)
  — the same model as upstream KDE Connect and the Android app. An active on-LAN attacker who MITMs the
  *first* connection during the pairing window is the known residual
  risk; the SAS comparison is the only defense, as upstream.
- **Peer certificate expiry is not enforced.** Authentication is TOFU
  fingerprint pinning, not PKI; an expired-but-fingerprint-matching cert
  is still the same device. The daemon's own certificates are
  regenerated on expiry.
- **Device identity broadcast.** Name, device ID, and capabilities are
  broadcast to the LAN via UDP every 60 seconds (default, configurable).
  This is inherent to the KDE Connect discovery protocol; do not run on
  networks where this disclosure is unacceptable.

## Mitigations that do exist

- TOFU fingerprint pinning enforced during the TLS handshake by custom
  certificate verifiers (not after the fact).
- Unpaired peers can only send pair/unpair packets; all other packet
  types are dropped.
- SAS pairing request lifetimes match Android's `PairingHandler`: the
  requester waits 30s for an accept (`PairingHandler.kt:151`) and the
  accepter holds a pending request 25s (`PairingHandler.kt:88`), so the
  accepter always gives up first. Separately, an incoming pair packet is
  rejected unless its timestamp is within a 1800-second freshness window.
- Received-file size caps on share transfers (default 100 MiB), pairing
  rate limits (max 10 concurrent pending), and packet size caps: 512 KiB
  for pre-auth identity reads (mirroring Android's `LanLinkProvider`) and
  32 MiB for steady-state general packets.
- Payload (file) transfers run over TLS, bounded to the declared
  `payloadSize`.
- REST API key authentication with constant-time comparison; key file
  written with owner-only permissions.
- systemd hardening directives in `packaging/rust-connect.service`:
  `ProtectSystem=strict` with explicit `ReadWritePaths`, `PrivateTmp`,
  `ProtectKernelTunables/Modules/Logs`, `ProtectControlGroups`,
  `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`,
  `RestrictRealtime`, `RestrictAddressFamilies`, `SystemCallArchitectures=native`,
  a `SystemCallFilter` allowlist, and `DeviceAllow` limited to
  `/dev/uinput` and `/dev/fuse`. The unit refuses to start under a
  display-manager greeter's user instance via `ConditionUser=!gdm-greeter`
  — a stale greeter user would mint a fresh identity and hold port 1716
  until login, racing real paired phones (audit 2026-09-02 §E).
  `NoNewPrivileges` and `RestrictSUIDSGID` are deliberately OFF: unprivileged
  users cannot `mount()` (needs `CAP_SYS_ADMIN`), so sshfs delegates to
  the setuid `fusermount3` helper, which requires privilege elevation to
  drop privileges. With either flag on, `fusermount3` core-dumps in
  `drop_privs` (observed live 2026-08-06). The daemon itself stays
  unprivileged; only the setuid helper it spawns can elevate. Keep the
  `SystemCallFilter` carve-out for `fusermount3`'s privilege drop in sync
  with the sftp-fuse drop-in documented in `docs/live-validation.md`;
  after a systemd upgrade, re-diff the `@privileged` syscall set
  against the unit line — drift silently breaks SFTP mounts
  (EPERM/SIGSYS) or widens the sandbox.
