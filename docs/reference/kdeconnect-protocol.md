# KDE Connect Protocol Reference

Protocol version: **8**

## Table of Contents

- [Packet Format](#packet-format)
- [Identity Packet (kdeconnect.identity)](#identity-packet-kdeconnectidentity)
- [Pair Packet (kdeconnect.pair)](#pair-packet-kdeconnectpair)
- [TLS Setup](#tls-setup)
- [Connection Flow](#connection-flow)
- [Android-specific Behavior](#android-specific-behavior)
- [Port Ranges](#port-ranges)
- [Implementation Gaps](#implementation-gaps)

---

## Packet Format

All communication consists of **JSON lines terminated by `\n`** (newline).

| Field                  | Type     | Required | Description                                     |
|------------------------|----------|----------|-------------------------------------------------|
| `id`                   | integer  | yes      | Millisecond timestamp (monotonic or wall clock) |
| `type`                 | string   | yes      | Packet type identifier (e.g. `kdeconnect.pair`) |
| `body`                 | object   | yes      | JSON object with packet-specific payload        |
| `payloadSize`          | integer  | no       | Size of binary payload in bytes                 |
| `payloadTransferInfo`  | object   | no       | Metadata for payload transfer (port, UUID, etc) |

Example:

```json
{"id":1700000000000,"type":"kdeconnect.pair","body":{"pair":true,"timestamp":1700000000}}\n
```

---

## Identity Packet

**Type:** `kdeconnect.identity`

Sent during connection establishment (once plaintext, once encrypted).

### Fields

| Field                   | Type    | Required | Description                                            |
|-------------------------|---------|----------|--------------------------------------------------------|
| `deviceId`              | string  | yes      | Unique device identifier, 32-38 chars, `[a-zA-Z0-9_-]` |
| `deviceName`            | string  | yes      | Human-readable name, max 32 chars, certain chars filtered |
| `deviceType`            | string  | yes      | One of: `desktop`, `laptop`, `phone`, `tablet`, `tv`   |
| `protocolVersion`       | integer | yes      | Must be `8`                                            |
| `incomingCapabilities`  | array   | yes      | List of packet types this device can receive            |
| `outgoingCapabilities`  | array   | yes      | List of packet types this device can send               |
| `tcpPort`               | integer | no       | TCP port for incoming connections (included in UDP only)|

### Device ID Rules

- Length: 32-38 characters
- Allowed characters: alphanumeric, underscore (`_`), hyphen (`-`)
- Typically a UUID without dashes (32 chars)

### Device Name Rules

- Maximum 32 characters **as emitted** — Android truncates its own name to 32
- Certain characters are filtered: `["',;:.!?()\[\]<>]` (DeviceHelper.kt:40)
- **Receive side:** Android never rejects on name length. It sanitizes
  received names (filter → trim → take 32) in
  `DeviceInfo.fromIdentityPacketAndCert`, and rejects only a name that is
  blank *after* sanitizing (`isValidIdentityPacket`)

---

## Pair Packet

**Type:** `kdeconnect.pair`

### Request

```json
{"pair": true, "timestamp": <unix_seconds>}
```

### Accept

```json
{"pair": true, "timestamp": <unix_seconds>}
```

In protocol version 8, both request and accept include `timestamp`.

### Reject / Unpair

```json
{"pair": false}
```

### Verification Key

The verification key displayed to the user is derived as:

```
SHA256(sorted_concat(certA_pubkey, certB_pubkey) + timestamp)
```

Truncated to the first **8 hex characters**.

The certificates are sorted lexicographically by their public key PEM before concatenation, ensuring both sides compute the same digest regardless of who initiated pairing.

### Timestamp Validation

- Reject the pair packet if `|pairing_timestamp - current_time| > 1800` seconds (30 minutes)
- This prevents replay attacks with stale pairing requests

### Timeouts

| Timeout                              | Duration | Description                            |
|--------------------------------------|----------|----------------------------------------|
| Waiting for peer accept              | 30s      | Time to wait for the other side's accept |
| Waiting for user response            | 25s      | Time to wait for local user confirmation |

---

## TLS Setup

### Role Reversal

The TLS roles are **reversed** relative to TCP:

- **TCP server** becomes **TLS client** (initiates the TLS handshake)
- **TCP client** becomes **TLS server** (presents the SSLServerSocket)

This means the device that accepts the TCP connection initiates the TLS handshake as a client.

### Certificate

| Property        | Value                                    |
|-----------------|------------------------------------------|
| Key type        | EC P-256 (modern) or RSA-2048 (legacy)  |
| CN              | `deviceId`                               |
| O               | `KDE`                                    |
| OU              | `KDE Connect`                            |
| Signature       | Self-signed, SHA-512                     |
| TLS version     | 1.2 (explicitly avoids 1.3)             |

### Certificate Validation Modes

#### Trusted Device

- `needClientAuth = true`
- Full certificate verification against the stored certificate
- Connection rejected if certificate does not match the stored one

#### Untrusted Device (not yet paired)

- `wantClientAuth = true`
- `trustAllCerts = true` — request certificate but do not verify it
- Self-signed certificate errors are silently ignored
- The certificate received is stored for later use during pairing

---

## Connection Flow

### Step-by-step

1. **UDP Discovery** — Broadcast/unicast on UDP port 1716 to discover peers
2. **TCP Connection** — Connect to peer on TCP port 1716-1764
3. **Plaintext Identity Exchange** — Both sides send `kdeconnect.identity` before TLS
4. **TLS Handshake** — Role-reversed TLS with certificate exchange
5. **Encrypted Identity Exchange** — Both sides re-send `kdeconnect.identity` over TLS
6. **Pairing** — If not already paired, exchange `kdeconnect.pair` packets
7. **Packet Exchange** — Normal encrypted communication

### Detailed TLS Connection Process

```
Device A (TCP client)          Device B (TCP server, port 1716-1764)
        |                               |
        |--- TCP SYN/SYN-ACK/ACK ----->|
        |                               |
        |<-- kdeconnect.identity ------>|  (plaintext)
        |--- kdeconnect.identity ----->|  (plaintext)
        |                               |
        |<===== TLS HANDSHAKE =======> |  (B is TLS server, A is TLS client)
        |                               |
        |<-- kdeconnect.identity ------>|  (encrypted, re-validate)
        |--- kdeconnect.identity ----->|  (encrypted, re-validate)
        |                               |
        |<-- kdeconnect.pair ---------->|  (if unpaired)
        |--- kdeconnect.pair ---------->|
```

### Important Notes

- The identity is exchanged **twice**: once plaintext (for initial identification), once encrypted (for verification over TLS)
- Protocol version must be checked during both identity exchanges
- Capabilities are used to determine which packet types each side supports

---

## Android-specific Behavior

### Certificate Storage

| Aspect             | Storage Detail                                                    |
|--------------------|-------------------------------------------------------------------|
| Certificate        | `SharedPreferences(<deviceId>)` → key `"certificate"` → Base64 DER |
| Trusted flag       | `SharedPreferences("trusted_devices")` → boolean per `deviceId`  |

### Pairing Lifecycle

| Event              | Actions                                                           |
|--------------------|-------------------------------------------------------------------|
| Pairing success    | Certificate persisted to SharedPreferences, trusted flag set     |
| Unpair             | Certificate removed, trusted flag cleared, device SharedPreferences cleared |

### Connection Management

- **Rate limit:** Minimum 1 second between connections to the same `deviceId`
- **Connection replacement:** If the same certificate is seen, `link.reset()` swaps the socket. If a different certificate is seen, the connection is aborted (potential MITM).

---

## Port Ranges

| Port / Range | Protocol | Purpose                              |
|---------------|----------|--------------------------------------|
| 1716 (UDP)    | UDP      | Discovery (broadcast/unicast)        |
| 1716-1764     | TCP      | Encrypted communication              |
| 1739+         | TCP      | File transfer payload transfer       |

The TCP listening port is typically chosen from 1716-1764. If the default port is unavailable, the implementation tries subsequent ports in the range. The actual listening port is advertised in UDP identity packets via the `tcpPort` field.

File transfer uses a separate TCP connection on a dynamically allocated port (typically starting at 1739), advertised in `payloadTransferInfo`.

---

## Implementation Gaps

### Comparison with Android Reference Implementation

| Aspect                 | Status  | Notes                                                       |
|------------------------|---------|-------------------------------------------------------------|
| Key algorithm          | WARNING | We use RSA-2048; Android expects EC P-256 — may cause issues |
| Certificate CN         | OK      | CN = deviceId (matches)                                      |
| TLS role reversal      | OK      | Correct in our code                                           |
| Pair packet format     | OK      | Matches v8 spec (includes timestamp)                          |
| Untrusted before pair  | OK      | `trusted:false` before pairing is expected behavior            |
| Phone reconnect timing | NOTE    | Reconnect every 5s — may be normal discovery behavior, investigate |

### Priority Concerns

1. **EC P-256 vs RSA-2048**: Android modern devices generate EC P-256 certificates. Our RSA-2048 certificates may cause compatibility issues with newer Android clients. Consider adding EC P-256 support and using it as the default.

2. **Phone reconnect frequency**: The 5-second reconnect cycle from phones is likely normal UDP discovery behavior rather than a bug, but should be verified against Android source.
