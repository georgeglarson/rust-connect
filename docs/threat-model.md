# Threat Model

Scope: the `rust-connect` daemon, its REST API, and its implementation of
the KDE Connect protocol (UDP/TCP 1716 discovery and transport, TLS,
pairing, payload transfers). Assets: paired-device trust, local files
(received shares, identity keys in `~/.local/share/rust-connect`), the
REST API control surface, and desktop session access (notifications,
clipboard, input injection, MPRIS).

## Adversary class A: unauthenticated LAN attacker

Anyone on the same broadcast domain; no credentials.

**Reach.** UDP discovery replies, inbound TCP on 1716, the TLS handshake,
and pair-request packets. They see identity broadcasts (name, device ID,
capabilities) sent every 60 s by default — inherent to the protocol.

**Mitigations.**

- Unpaired peers can only send pair/unpair packets; every other packet
  type is dropped before reaching plugins.
- TOFU SHA256 fingerprint pinning enforced during the TLS handshake by
  custom certificate verifiers (`src/protocol/connection/tls.rs`).
- Pairing acceptance itself is refused without a peer certificate —
  either presented on the live session or already pinned — so a
  pairing can never exist without an identity anchor
  (`src/protocol/pairing/mod.rs` `has_identity_anchor`, vk #1056).
- Pairing requires a human-confirmed 32-bit SAS (Short Authentication
  String) with a 1800 s freshness window, matching Android's
  `PairingHandler`; pairing rate-limited to 10 concurrent pending.
- Pre-auth identity reads capped at 512 KiB (mirrors Android
  `LanLinkProvider`).
- Per-IP outbound connection rate limit throttles dials triggered by
  spoofed discovery responses.

**Residual risks.** An active MITM during the first-connection pairing
window can attempt to race the SAS comparison — the SAS is the only
defense, as upstream. Spoofed identity broadcasts can cause outbound
dials to attacker hosts (rate-limited, and no trust is gained without
pairing). LAN discovery disclosure cannot be disabled without disabling
discovery.

## Adversary class B: malicious paired device

A device that completed pairing — e.g. a compromised phone or a device
paired under class-A MITM.

**Reach.** The full paired packet surface: every plugin the device
advertised capabilities for, plus payload (file) transfers.

**Mitigations.**

- Received-file size caps (default 100 MiB) and payload transfers
  bounded to the declared `payloadSize`, over TLS.
- Packet size caps (512 KiB pre-auth identity reads, 32 MiB steady-state)
  and per-connection rate limits bound resource exhaustion.
- Plugins only honor capabilities negotiated at handshake; runcommand
  and input injection are limited to what the local configuration
  exposes.

**Residual risks.** Pairing is the trust boundary — a paired device can
inject input, read notifications, and push files within the caps.
Resource exhaustion is only partially mitigated by the size and rate
caps. Pair deliberately; unpair aggressively.

## Adversary class C: malicious local process

Code running as the same user (or root) on the host.

**Reach.** The REST API, the identity key material, the session D-Bus.

**Mitigations.**

- REST API binds 127.0.0.1 by default — not reachable off-host unless
  explicitly reconfigured.
- API key authentication with constant-time comparison; the key is
  auto-generated, stored owner-only (mode 0600), and never logged.
- Per-IP API rate limiting.
- systemd hardening in `packaging/rust-connect.service`:
  `NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict`,
  `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`,
  `ProtectHostname`.

**Residual risks.** A same-user process can read the API key and
identity keys from disk — this is inherent to running unprivileged; the
API key is the only barrier, and it is weak against a local reader. If
the API is rebound to a LAN address, weak key handling turns it into a
remote control surface: generate a fresh key and treat it as a secret.
systemd hardening limits post-compromise damage, not compromise.
