# Live device validation

Results from hands-on testing against real hardware, newest first. The
point of this file: protocol-conformance claims in this repo are backed by
observed behavior against real devices, not only by loopback tests.

## 2026-08-02 — Android phone (stock KDE Connect app `org.kde.kdeconnect_tp`)

Build under test: post-hardening `main` (rcgen certificate stack, SHA-512
signing, no SANs; API on 127.0.0.1; full hardening pass). Host: Linux,
daemon as systemd user service. Networks exercised: Wi-Fi LAN and USB
tether (rndis) — the phone used both during the session.

### Upgrade continuity

- The phone was paired to the pre-hardening daemon (openssl stack). After
  swapping in the new binary and restarting, the phone reconnected on its
  own: the existing TOFU pairing survived the certificate-stack migration.
- On first init the daemon tightened the pre-existing certificate
  directory from 0755 to 0700 and verified `own.key` at 0600 — upgrade
  path for loose installs works as designed.
- After an unplanned host power-cycle, the daemon came back via systemd
  linger and the phone reconnected without intervention.

### Feature matrix

| Flow | Result | Evidence |
|---|---|---|
| Discovery + connect | PASS | phone discovered over UDP broadcast on both LAN and USB-tether |
| Ping | PASS | API `POST /devices/:id/ping` |
| Battery | PASS | live values: 90%, charging |
| Connectivity | PASS | signal strength reported |
| Clipboard desktop→phone | PASS | text pasted on phone |
| Clipboard phone→desktop | PASS (with platform caveat) | Android 10+ only syncs clipboard while the KDE Connect app is foreground or via its send-clipboard tile — see docs/troubleshooting.md |
| Share desktop→phone | PASS | received intact with correct filename |
| Share phone→desktop | PASS | 81,127-byte PNG received over payload TLS to `~/Downloads`, basename preserved |
| Notification desktop→phone | PASS | appeared on phone |
| Notification mirror phone→desktop | PASS | Digital Wellbeing notification mirrored |
| Unpair | PASS | both sides severed, `pair=false` processed |
| Fresh pair with SAS | PASS | SAS displayed by API matched the phone's pairing dialog; accept completed pairing; peer cert re-stored and fingerprint-verified on next handshake |

### Pairing UX findings (now documented in docs/troubleshooting.md)

- Android surfaces an incoming pair request as a *silent* notification
  (Accept/Reject actions) in the notification shade, not a pop-up.
- Pairing windows are short: 25 s incoming, 30 s outgoing. An expired
  request just needs a fresh one.
- `POST /devices/:id/unpair` is a `DELETE`, not a `POST`.

### Evidence artifacts

Raw command output and a phone screenshot from this session are archived
outside the repo (session transcripts, `adb exec-out screencap`).
