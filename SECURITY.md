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
  shown on both devices during pairing — the same model as upstream KDE
  Connect and the Android app. An active on-LAN attacker who MITMs the
  *first* connection during the pairing window is the known residual
  risk; the SAS comparison is the only defense, as upstream.
- **Peer certificate expiry is not enforced.** Authentication is TOFU
  fingerprint pinning, not PKI; an expired-but-fingerprint-matching cert
  is still the same device. The daemon's own certificates are
  regenerated on expiry.
- **Device identity broadcast.** Name, device ID, and capabilities are
  broadcast to the LAN via UDP every 5 seconds. This is inherent to the
  KDE Connect discovery protocol; do not run on networks where this
  disclosure is unacceptable.

## Mitigations that do exist

- TOFU fingerprint pinning enforced during the TLS handshake by custom
  certificate verifiers (not after the fact).
- Unpaired peers can only send pair/unpair packets; all other packet
  types are dropped.
- SAS pairing with a 1800-second freshness window, matching Android's
  `PairingHandler`.
- Received-file size caps on share transfers (default 100 MiB), pairing
  rate limits (max 10 concurrent pending), and a 512 KiB packet size
  cap, mirroring Android's `LanLinkProvider`.
- Payload (file) transfers run over TLS, bounded to the declared
  `payloadSize`.
- REST API key authentication with constant-time comparison; key file
  written with owner-only permissions.
- systemd hardening directives in `packaging/rust-connect.service`
  (`NoNewPrivileges`, `ProtectSystem=strict`, and friends).
