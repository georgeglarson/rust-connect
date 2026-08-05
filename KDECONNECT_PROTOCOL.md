# KDE Connect Protocol Implementation Guide

## Overview

This guide documents the KDE Connect network protocol based on analysis of three working implementations:
- **konnect** (Python/Twisted)
- **pykdeconn** (Python/anyio)
- **kdeconnect-mcp-server** (Python/D-Bus)

*(Reference clones retired 2026-06; the authoritative conformance sources are
the Android upstream files committed in `docs/reference/`.)*

## Architecture Decision: Two Approaches

### Approach A: D-Bus Wrapper (Simplest)
Use the official KDE Connect daemon via D-Bus. No protocol implementation needed.
- Reference: `kdeconnect-mcp-server/mcp_server.py`
- Pros: Works immediately, no protocol bugs
- Cons: Requires official KDE Connect desktop app installed

### Approach B: Protocol Reimplementation (What rust-connect does)
Implement the full KDE Connect network protocol from scratch.
- References: `konnect/protocols.py`, `pykdeconn/pyconnect.py`
- Pros: Standalone, no dependencies
- Cons: Complex, many edge cases

---

## Protocol Specification

### Constants
```
MIN_TCP_PORT = 1716
MAX_TCP_PORT = 1764
PROTOCOL_VERSION = 7 (or 8 for newer Android)
UDP_PORT = 1716
BUFFER_SIZE = 8192  # max packet size before TLS
TIMESTAMP_DIFFERENCE = 1800  # 30 minutes
```

### Packet Format
All packets are JSON followed by `\n`:
```json
{"id": <unix_timestamp_ms>, "type": "kdeconnect.<plugin>.<action>", "body": {<key>: <value>}}
```

### Required Packet Fields
- `id`: integer (unix timestamp in milliseconds)
- `type`: string (packet type identifier)
- `body`: object (packet-specific data)

### Identity Packet
**Plaintext** (before TLS):
```json
{
  "id": 0,
  "type": "kdeconnect.identity",
  "body": {
    "deviceId": "<uuid-urn>",
    "deviceName": "<hostname>",
    "deviceType": "laptop|phone|tablet|desktop",
    "protocolVersion": 7,
    "incomingCapabilities": ["kdeconnect.ping", "kdeconnect.notification.request", ...],
    "outgoingCapabilities": ["kdeconnect.notification", "kdeconnect.ping", ...],
    "tcpPort": 1716
  }
}
```

**CRITICAL**: The identity packet MUST include ALL six fields in body:
- `deviceId`, `deviceName`, `deviceType`, `protocolVersion`
- `incomingCapabilities` (array of strings)
- `outgoingCapabilities` (array of strings)
- `tcpPort` (integer)

### Pair Packet
```json
{
  "id": <timestamp_ms>,
  "type": "kdeconnect.pair",
  "body": {
    "pair": true|false,
    "timestamp": <unix_timestamp_seconds>
  }
}
```

---

## Connection Flow

### Phase 1: Discovery (UDP + mDNS)

**UDP broadcast (fallback):**
1. Both devices broadcast identity packets to the LAN broadcast address, UDP
   port 1716 (interval configurable, 60s default)
2. On receiving identity from a **paired** device, initiate TCP connection
3. On receiving identity from **unknown** device, just log it (don't connect)

**mDNS / DNS-SD (primary in both reference implementations):**
1. Both devices register a `_kdeconnect._udp.local` service (yes, `_udp`,
   even though the data connection is TCP — kdeconnect-kde
   `mdnshdiscovery.cpp:15`, Android `MdnsDiscovery.kt` `SERVICE_TYPE`)
2. Instance name = `deviceId`; service port = the TCP port peers should
   dial; TXT records `id`, `name`, `type`, `protocol` (no capability
   lists — they arrive with the TCP identity exchange)
3. On resolve, the resolved address + port + TXT are enough to dial
   directly (the references instead send a UDP identity to the resolved
   address and let the peer dial; both carry a TODO noting the v8 dial-
   direct path, which is what rust-connect implements)
4. Announcement is unregistered (mDNS goodbye) on daemon shutdown

### Phase 2: TCP Connection + TLS Handshake

**Incoming connection (phone → us):**
1. Phone connects to our TCP port (1716)
2. Phone sends **plaintext identity** (no TLS yet)
3. We read plaintext identity, extract `deviceId`
4. We call `startTLS` with `server_side=False` (we're the TLS client, phone is TLS server)
   - **CRITICAL**: The phone initiated the TCP connection but expects to be the TLS server
   - This is "role reversal" - TCP server becomes TLS client
5. After TLS handshake, we read **encrypted identity** from the TLS stream
6. We send our encrypted identity back
7. Verify peer certificate CN matches `deviceId` from plaintext identity

**Outgoing connection (us → phone):**
1. We connect to phone's TCP port (from UDP identity `tcpPort` field)
2. We send **plaintext identity**
3. Wait 500ms for phone to process
4. We call `startTLS` as TLS client
5. After TLS, exchange encrypted identities

### Phase 3: Pairing (CRITICAL - This is where rust-connect fails)

**State machine:**
```
NOT_PAIRED → REQUESTED → PAIRED
```

**When we receive `{"pair": true}`:**
1. If `status == REQUESTED` (we sent a pair request):
   - Extract peer certificate from TLS stream
   - Save certificate PEM to disk
   - Mark `status = PAIRED`
   - Save to database
   - **Send `{"pair": true}` back** (confirmation)

2. If `status == PAIRED` (already paired, re-confirming):
   - Update device info in database
   - **Send `{"pair": true}` back** (re-confirm)

3. If `status == NOT_PAIRED` (incoming pair request):
   - **Send `{"pair": true}` back** (auto-accept)
   - Extract peer certificate
   - Mark `status = PAIRED`
   - Save to database

**When we receive `{"pair": false}`:**
- Mark `status = NOT_PAIRED`
- Remove from database
- Close connection

**CRITICAL RULE**: **ALWAYS respond to a pair packet with a pair packet.** The phone waits ~3 seconds for a response. If it doesn't get one, it disconnects.

### Phase 4: Packet Exchange (After Pairing)

Only process packets from **paired/trusted** devices. For unknown devices:
- Send `{"pair": false}` back
- Close connection

**Supported packet types** (the 24 plugins registered in `src/plugins/loader.rs`; several share a packet type, so the roster is longer than this list):
- `kdeconnect.ping` → Respond with ping
- `kdeconnect.notification` / `kdeconnect.notification.request` → Notification sync
- `kdeconnect.share.request` → File transfer, shared text, or a shared URL.
  Exactly one of `filename` (with a payload), `text`, or `url` is present;
  the branch order matches upstream (shareplugin.cpp:119,158,232).
  A text share is staged in the download dir as `kdeconnect-XXXXXXXX.txt`;
  a URL share is opened with `xdg-open` when its scheme is in the allowlist
  (http, https, ftp, ftps, mailto, tel) and surfaced either way.
- `kdeconnect.share.request.update` → Batch totals (`numberOfFiles`,
  `totalPayloadSize`) for a multi-file transfer. Consumed, never sent: a
  single-file-per-call send API has no batch to report on, and adding the
  keys to an outgoing request would make Android fold independent sends into
  one progress job (SharePlugin.java:258-279).
- `kdeconnect.clipboard` (both ways; body `{"content": "<text>"}`, no timestamp, applied unconditionally on receipt — kdeconnect-android ClipboardPlugin.kt:77-81, :151-160) / `kdeconnect.clipboard.connect` (both ways; body `{"content": "<text>", "timestamp": <ms since epoch>}`, sent once on connect — ClipboardPlugin.kt:83-98, :162-177; receivers ignore timestamp 0 or older than their last update — ClipboardPlugin.kt:48-52, kdeconnect-kde clipboardplugin.cpp:188-194) → Clipboard sync. REAL both ways via wl-clipboard: phone→desktop writes the session clipboard with `wl-copy`; desktop→phone watches with a persistent `wl-paste --watch` notifier + per-change `wl-paste -n` reads (GSConnect's mechanism, gsconnect src/wl_clipboard.js:67-74, 208) and fans out to connected devices. Echo loops suppressed by content equality (state primed before our own wl-copy write — clipboardlistener.cpp:112-118, gsconnect clipboard.js:163-168 vs :144). Empty selections never propagate. Degrades to store-and-event only (logged) when no Wayland session/wl-clipboard; the backend is enabled only at the production entry point (bootstrap.rs), never in tests. `kdeconnect.clipboard.file` (file/image payloads) is not implemented and not advertised.
- `kdeconnect.battery` / `kdeconnect.battery.request` → Battery status
- `kdeconnect.mpris` (both ways; desktop→phone carries `{"playerList": [...], "supportAlbumArtPayload": false}` and per-player updates keyed by display name — changed fields only, plus `player`, always `canSeek`, and `pos` when seekable, kdeconnect-kde mpriscontrolplugin.cpp:137-195,387-394; metadata mapping title/artist/album/albumArtUrl/url/length in ms at :396-425; Seeked → `{"pos": <ms>, "player"}` at :101-120) / `kdeconnect.mpris.request` (both ways; phone→desktop commands: `action` PlayPause/Play/Pause/Next/Previous/Stop relayed to the bus, `setVolume` 0-100 int → 0.0-1.0, `Seek` in MICROSECONDS passed through unchanged (:303-307; Android's default is ±10000000µs, strings.xml:277), `SetPosition` absolute ms ×1000 via SetPosition(trackid) with seek-by-difference fallback (gsconnect mpris.js:246-259), `setLoopStatus`/`setShuffle`, `requestPlayerList`, `requestNowPlaying`+`requestVolume` answered with full state (:255-358)) → Media player control. REAL both ways via zbus on the session D-Bus: we discover `org.mpris.MediaPlayer2.*` players, display-name them from MPRIS Identity with prefix-strip fallback and ` [2]` dedup (:74-92), ignore playerctld and `kdeconnect.*` players (:55-61), and apply the plasma-browser-integration filtering (:361-385). Upstream has no "current player" concept — all players are tracked and the phone picks by name. Album-art payloads are not implemented, so `supportAlbumArtPayload` is `false` and `albumArtUrl` requests are dropped (upstream: :217-253). Degrades (logged) to an empty player list when no session bus; the backend is enabled only at the production entry point (bootstrap.rs), never in tests. Phone-as-player updates (remote role) are stored per device and pulled with requestNowPlaying/requestVolume as before.
- `kdeconnect.mousepad.request` (incoming) → Remote input, injected via uinput. One packet carries one intent, and the click/scroll/key chain is mutually exclusive with pointer movement (kdeconnect-kde plugins/mousepad/x11remoteinput.cpp:113-198, mirrored waylandremoteinput.cpp:458-525). Clicks: `singleclick` / `doubleclick` / `middleclick` / `rightclick` / `singlehold` / `singlerelease`, all lowercase bools, one per packet (kdeconnect-android MousePadPlugin.kt:88-122); `singlehold`+`singlerelease` are the drag'n'drop pair (x11remoteinput.cpp:132-136). Scroll: `{"scroll": true, "dx": f, "dy": f}` reinterprets the deltas as wheel deltas, one notch per packet in the sign of `dy` with magnitude carrying no notch count (x11remoteinput.cpp:137-144; the sender accumulates and thresholds, MousePadActivity.java:398-403); `dx` is always 0 from Android (MousePadActivity.java:576) and maps to REL_HWHEEL for desktop peers (waylandremoteinput.cpp:479). Movement: `{"dx": f, "dy": f}` truncated to whole pixels (MousePadPlugin.kt:77-82, x11remoteinput.cpp:192-193). Keys: `key` (a string of characters) or `specialKey` (an INTEGER code 1..32, table at x11remoteinput.cpp:27-61 and KeyListenerView.java:26-61, with 17-20 the four modifier keys), held under four INDEPENDENT modifier bools `ctrl` / `alt` / `shift` / `super` (x11remoteinput.cpp:146-158) — so Ctrl+Shift+A is one packet with two bools set. Absolute `x`/`y` is decoded, logged and dropped: we register relative axes only, and no client sends it over the wire (kdeconnect-kde's only producer delivers it in-process, shareinputdevicesremoteplugin.cpp:74-75). There is no `button` field in this protocol.
- `kdeconnect.mousepad.keyboardstate` (outgoing) → Sent once on connect, body `{"state": <bool>}`, reporting whether we can inject keystrokes (kdeconnect-kde mousepadplugin.cpp:63-70). The Android app greys out its keyboard button when false (MousePadPlugin.kt:26-29). We report whether the uinput device opened; upstream omits the field when it has no backend and Android then defaults to true.
- `kdeconnect.lock` / `kdeconnect.lock.request` → Remote lock/unlock
- `kdeconnect.sftp` / `kdeconnect.sftp.request` → Filesystem browse
- `kdeconnect.systemvolume` / `kdeconnect.systemvolume.request` → Volume control
- `kdeconnect.sms.messages` / `kdeconnect.sms.request` → SMS sync
- `kdeconnect.telephony` → Call state
- `kdeconnect.connectivity_report` → Network status (INCOMING ONLY, push-driven: the phone sends it on radio-state change, kdeconnect-android .../connectivityreport/ConnectivityReportPlugin.kt:51-68). There is no working `.request`: Android declares `supportedPacketTypes = emptyArray()` (:84) and returns false from `onPacketReceived` (:80-82), and kdeconnect-kde's manifest declares `"X-KdeConnect-OutgoingPacketType": []` (plugins/connectivity-report/kdeconnect_connectivity_report.json:155). The type appears only in prose in kde's plugins/connectivity-report/README:10, so we neither advertise nor send it.
- `kdeconnect.presenter` → Presentation pointer (`{"dx": float, "dy": float}` relative movement, `{"stop": true}` ends pointer mode; slide next/previous/fullscreen/esc arrive as `kdeconnect.mousepad.request` specialKey packets — kdeconnect-android PresenterPlugin.kt:53-88)
- `kdeconnect.findmyphone.request` → Ring a paired device (outgoing-only, empty body — kdeconnect-kde plugins/findmyphone/findmyphoneplugin.cpp:17-21)
- `kdeconnect.contacts.request_all_uids_timestamps` (outgoing, empty body — kdeconnect-kde plugins/contacts/contactsplugin.cpp:169-176) / `kdeconnect.contacts.response_uids_timestamps` (incoming; body `{"uids": [...], "<uid>": "<timestamp-as-string>"}` — kdeconnect-android ContactsPlugin.kt:110-119) / `kdeconnect.contacts.request_vcards_by_uid` (outgoing, body `{"uids": [...]}` — contactsplugin.cpp:178-185) / `kdeconnect.contacts.response_vcards` (incoming; body `{"uids": [...], "<uid>": "<raw vCard>"}` — ContactsPlugin.kt:140-155) → Contacts sync. Sync flow mirrors kdeconnect-kde (contactsplugin.cpp:64-134): fetch vCards only for new/changed uids, drop contacts the phone stops reporting.
- `kdeconnect.runcommand` (outgoing; body `{"commandList": "<JSON string of {key: {name, command}}>", "canAddCommand": bool}` — kdeconnect-kde plugins/runcommand/runcommandplugin.cpp:161-168, sent on connect per runcommandplugin.cpp:156-159; Android parses commandList as a string, RunCommandPlugin.java:155) / `kdeconnect.runcommand.request` (incoming; body `{"requestCommandList": true}` → answer with the advertisement, or `{"key": "<key>"}` → execute if allowlisted — RunCommandPlugin.java:242-254, runcommandplugin.cpp:50-68) → Remote command execution. Execution via `/bin/sh -c` like upstream (runcommandplugin.cpp:34-37, 102). The production allowlist is EMPTY: the advertised command list is `{}` and all requests are refused. `kdeconnect.runcommand.output` streaming is not implemented and not advertised.
- `kdeconnect.digitizer.session` / `kdeconnect.digitizer` (incoming) → Drawing-tablet input. Session packets open and close a stylus session; the movement packets carry the pen coordinates. Outgoing capability is empty.
- `kdeconnect.mousepad.echo` (incoming) → Keystroke echo for the remote-keyboard plugin, which sends `kdeconnect.mousepad.request` in the desktop-as-controller direction (the mousepad plugin handles the same packet type in the desktop-as-target direction; the router fans out to both).
- `kdeconnect.notification.request` (incoming) → Handled twice, by the notification plugin for reply/dismiss requests from the phone, and by the sendnotifications plugin, which pushes desktop notifications to the phone as `kdeconnect.notification`.

**The full 24-plugin roster** (the registration order, which is the order
`PluginAccess::all()` yields in `src/plugins/mod.rs` — `load_all` in
`src/plugins/loader.rs` registers them in exactly that sequence; the
`PluginAccess` struct literal in `load_default_plugins` is a construction
order and deliberately differs): ping, battery,
notification, sms, clipboard, share, mpris, telephony, pausemusic,
connectivity, sftp, mousepad, lock, systemvolume, findmyphone,
findthisdevice, presenter, contacts, runcommand, sendnotifications,
remotekeyboard, digitizer, screensaver-inhibit, remotecommands.

Four of those register no packet type of their own and so do not appear
in the list above. `pausemusic` listens on `kdeconnect.telephony`
alongside the telephony plugin and pauses local media for the duration of
a call. `findthisdevice` listens on `kdeconnect.findmyphone.request` and
rings the desktop, the mirror of `findmyphone` ringing the phone.
`screensaver-inhibit` advertises nothing at all: it acts on connect and
disconnect, holding a session inhibit while a device is linked.
`remotecommands` is the desktop-as-controller half of runcommand, so it
receives `kdeconnect.runcommand` advertisements from the phone and sends
`kdeconnect.runcommand.request`.

---

## Common Bugs in rust-connect

### Bug 1: Pair Response Missing
**Symptom**: Phone connects, identity exchange completes, then disconnects after ~3 seconds.
**Cause**: Phone sends `{"pair": true}`, we store it but never respond.
**Fix**: Always send `{"pair": true}` back when receiving a pair packet.

### Bug 2: Pair State Mismatch
**Symptom**: Phone rejects pairing even after user pairs on device.
**Cause**: We check `is_paired` but don't respond with pair confirmation.
**Fix**: When receiving `{"pair": true}` and already paired, send `{"pair": true}` back.

### Bug 3: No Response to Unpair
**Symptom**: Phone sends `{"pair": false}`, we log it but don't clean up.
**Cause**: `reject_pairing()` only clears pending requests, doesn't unpair.
**Fix**: When receiving `{"pair": false}`, call `unpair()` to clear stale state.

### Bug 4: Identity Packet Missing Capabilities
**Symptom**: Phone doesn't know what we support.
**Cause**: Identity packet missing `incomingCapabilities`/`outgoingCapabilities`.
**Fix**: Include all capability arrays in identity packet.

---

## Reference File Map

### konnect (`~/src/reference/konnect/konnect/`)
| File | Purpose | Key Lines |
|------|---------|-----------|
| `protocols.py` | Main protocol handler | `_handleIdentity()` L124-140, `_handlePairing()` L142-174 |
| `packet.py` | Packet structure | `createIdentity()` L78-91, `createPair()` L94-99 |
| `server.py` | TCP/UDP server setup | Discovery broadcast, TCP listener |
| `certificate.py` | TLS cert management | PEM generation, loading |

### pykdeconn (`~/src/reference/pykdeconn/pyconnect.py`)
| Section | Purpose | Key Lines |
|---------|---------|-----------|
| `outgoing_connection_task()` | Outgoing TCP+TLS | L540-620 |
| `incoming_connection_task()` | Incoming TCP+TLS | L623-720 |
| `handle_pairing()` | Pairing state machine | L780-830 |
| `handle_packet()` | Packet dispatch | L730-770 |
| `DeviceConfig.ssl_context()` | TLS context setup | L310-340 |

### kdeconnect-mcp-server (`~/src/reference/kdeconnect-mcp-server/`)
| File | Purpose |
|------|---------|
| `mcp_server.py` | D-Bus wrapper for KDE Connect |
| `SETUP.md` | Installation instructions |

### kdeconnect-android (`~/src/reference/kdeconnect-android/`)
| File | Purpose |
|------|---------|
| `Device.kt` | Device state management, packet routing |
| `plugins/Plugin.kt` | Plugin base class |
| `PairingHandler.kt` | Pairing state machine |

### kdeconnect-kde (`~/src/reference/kdeconnect-kde/`)
| File | Purpose |
|------|---------|
| `core/backends/lan/lanlinkprovider.cpp` | TCP/UDP discovery, connection handling |
| `core/backends/lan/landevicelink.cpp` | TLS handshake, packet send/recv |
| `core/device.cpp` | Device state, plugin management |
| `plugins/ping/pingplugin.cpp` | Simple plugin example |

---

## TLS Certificate Requirements

1. **Certificate CN** must be the device ID (UUID URN format: `e0f7faa7-...`)
2. **Certificate format**: PEM encoded X.509
3. **Peer certificate**: Must be stored and verified on first pairing
4. **Certificate verification**: CN must match `deviceId` from identity packet

### Certificate Generation
```bash
openssl req -new -x509 -sha256 -out cert.pem -newkey rsa:4096 -nodes \
  -keyout key.pem -days 3650 \
  -subj "/O=kdeconnect/OU=Device/CN=<device-id-uuid>"
```

---

## Testing Checklist

- [ ] UDP discovery broadcasts every 2 seconds
- [ ] TCP listener on port 1716
- [ ] Plaintext identity read from incoming connections
- [ ] TLS role reversal (TCP server → TLS client)
- [ ] Encrypted identity exchange
- [ ] Peer certificate CN verification
- [ ] Pair request sent to new devices
- [ ] **Pair response sent when receiving `{"pair": true}`** (CRITICAL)
- [ ] Pair rejection handled when receiving `{"pair": false}`
- [ ] Stale pairing state cleared on rejection
- [ ] Only paired devices can send plugin packets
- [ ] Unsupported packets from unpaired devices get `{"pair": false}` response
- [ ] Connection stays open >30 seconds (no timeout)
- [ ] Reconnection after disconnect works
