# Live device validation

Results from hands-on testing against real hardware, newest first. The
point of this file: protocol-conformance claims in this repo are backed by
observed behavior against real devices, not only by loopback tests.

## 2026-08-05 — second Android device, v0.1.0 release build

First run against a *different* handset than every session below, so this is
device-diversity coverage rather than a repeat: a Samsung SM-G998U1 on the
stock KDE Connect Android app, host on Wi-Fi LAN (no USB tethering; adb used
only to read the phone's `wlan0` address).

Build under test: the released v0.1.0 tree.

### Packaged install, from the published artifacts

- `.deb` and binary downloaded from the release and checksum-verified against
  the published `SHA256SUMS`.
- Installed to `/usr/bin` with the systemd **user** unit from
  `/usr/lib/systemd/user/`, started with
  `systemctl --user enable --now rust-connect.service`.
- First start on a machine with no data dir exposed a packaging bug: with
  `ProtectSystem=strict`, a `ReadWritePaths` target that does not exist yet
  fails namespace setup (`status=226/NAMESPACE`) before the daemon can run to
  create it. Fixed with an `ExecStartPre=+` mkdir; the release was re-cut and
  the shipped unit re-verified.
- Fresh identity minted on first start; `api_key` written `0600` as documented.

### Inbound connection path (`usb_android_connects_to_us`)

Automated, unattended:

- Discovery broadcast sent; the phone dialed the host back from its LAN address.
- Inbound accept succeeded, phone identified as a Galaxy S21 Ultra 5G.
- Mutual TLS completed with the phone as TLS client, encrypted identity
  exchange completed, link still alive at the end of the exchange.

Prerequisite worth repeating: the installed daemon holds port 1716, so it must
be stopped before running this suite.

### Desktop-initiated pairing and post-pairing traffic (`usb_full_protocol_handshake`)

- Direct dial to the phone, pair request sent, phone accepted.
- **SAS matched on both sides** (compared by hand against the phone's dialog).
- The encrypted link then carried real plugin traffic both ways: ping and a
  battery request out, a `kdeconnect.battery` packet back.

Note on running this by hand: the accept window is ~30 s from the request, and
on Android the request arrives as a *silent* notification, so it is easy to
miss. Have the KDE Connect app open before starting the test.

### File transfer (`usb_send_file_to_android`)

- Phone dialed the host, inbound handshake and encrypted identity exchange
  completed, pairing confirmed.
- 1 MiB payload sent over TLS on port 1739 and accepted by the phone.
- Receipt confirmed on the device: the file was present in the phone's
  Downloads afterwards. Worth stating separately, because the test itself only
  proves the payload was sent, not that the phone wrote it.

### Phone-initiated pairing: SAS verified identical on both devices

Run against the fixed build after the accept-side SAS work landed.

- Phone sent a pair request; the daemon surfaced it as `requested_by_peer`
  with a verification key, and reading the device over the REST API did **not**
  consume or accept the request.
- **The desktop and the phone displayed the same key**: `65D58104` on the API
  and CLI, and `65D58104` in the Android app's "Pair requested" screen,
  captured from the device screen rather than transcribed by hand.
- The daemon journal carried the same value under `pair_request_sas`.
- An earlier accept in the same session completed the pairing with its key
  (`589FCFC9`) shown before the accept, so the display is on the path that
  actually pairs, not a side channel.

This is the assertion the SAS exists for: not that a key is displayed, but
that it is the *same* key both sides derived. Before the fix, the accepting
side displayed nothing at all on this path.

Also observed: the incoming request's ~25 s window is genuinely short. Letting
it lapse made the CLI fall through to a fresh outgoing request and report a
timeout — correctly, rather than reporting a success that did not happen.

### Pairing

- Phone-initiated pairing completed against the packaged daemon.
- **Finding: the desktop side displayed no SAS on this path.** Accepting an
  incoming request through the CLI completed the pairing without ever showing
  the verification key, so there was nothing to compare against the phone's
  dialog. The SAS was only surfaced for desktop-initiated pairing. Being fixed
  separately; the parity claim in `SECURITY.md` was accurate only in the
  outgoing direction when this session ran.

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

## 2026-08-06 — systemvolume provider live validation (A15, dev daemon)

Session against the Task 1.1 dev build with wire transcript recording
(`RUST_CONNECT_TRANSCRIPT_DIR`, per-device jsonl with direction + timestamps).

| Flow | Result | Evidence |
|---|---|---|
| Fresh pair, phone-initiated | PASS | phone dialog SAS `E8FC00CB` == daemon `verification_key`; accept inside the 25 s window completed pairing |
| Provider caps advertised | PASS | identity outgoing includes `kdeconnect.systemvolume`, incoming `kdeconnect.systemvolume.request` (backend: pactl, 2 sinks) |
| Phone lists desktop sinks | PASS | Multimedia control → Devices tab rendered both sinks with correct default radio and volumes (45% / 50%, matched pactl) |
| Phone → desktop volume | PASS | slider tap → `kdeconnect.systemvolume.request {name, volume}` → `pactl get-sink-volume` 49280 / 75% |
| Phone → desktop mute | PASS | speaker icon → `request {muted: true}` → `pactl get-sink-mute` yes |
| Desktop → wire deltas | PASS | pactl volume/mute changes pushed as sparse deltas; full sinkList pushed on connect and on sink-set change (transcript dir=out) |
| REST ↔ pactl parity | PASS | `GET /api/v1/systemvolume/sinks` tracked live pactl; `POST .../control` moved pactl |

### Findings

- **The stock Android app never sends `requestSinks`** (kdeconnect-android
  @ a88f6fa0 has no sender; the Devices tab renders whatever sinkList the
  desktop pushes). Upstream kdeconnect-kde pushes on connect; rust-connect
  now does the same via the capability-gated peer sync (`sync_peers`), which
  also ended the controller role's blind `requestSinks` spam at non-provider
  peers.
- **pipewire-pulse subscribe dialect** differs from PulseAudio: sink events
  carry no quoted name (`Event 'change' on sink #68`), suspended-sink
  volume/mute changes can surface only as `card` events, and default-sink
  moves arrive as `server` events. The parser classifies all three; test
  fixtures are live captures, not hand-typed.
- **Phone-app caveat (observed, not a rust-connect defect):** after the
  initial sinkList render, the stock app's Devices tab did not visibly apply
  subsequent sparse deltas (slider kept its optimistic position; logcat
  silent). Our deltas match upstream's shape exactly
  (systemvolumeplugin-pulse.cpp:69-88). Phone→desktop control is PASS;
  desktop→phone UI re-render stays UNVERIFIED in the ledger.
