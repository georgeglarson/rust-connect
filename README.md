# Rust Connect

A modern, API-first reimplementation of KDE Connect in Rust, compatible with the existing KDE Connect Android app.

## Quick Start

Download the `.deb` from the [latest release](https://github.com/georgeglarson/rust-connect/releases/latest) and install it:

```bash
sudo apt install ./rust-connect_0.1.0_amd64.deb
systemctl --user enable --now rust-connect.service
```

The package ships a systemd **user** unit (see below for why it must be a
user service, not a system one). The second command starts it for your user
right away; for other users it starts at their next login.

No Debian-based distro? Grab the `rust-connect` binary from the same release
(x86-64 Linux, glibc 2.17+), or build from source. The binary also links
**libxkbcommon** at load time, which the `.deb` pulls in for you but a bare
binary does not — install it first if your system is minimal (Fedora:
`dnf install libxkbcommon`; Arch: `pacman -S libxkbcommon`). Without it the
binary exits with `error while loading shared libraries: libxkbcommon.so.0`.

```bash
cargo run
```

On first run an API key is auto-generated and written to
`~/.local/share/rust-connect/api_key` with owner-only permissions (0600).
It is deliberately never logged. CLI subcommands read that file
automatically; for direct REST API calls, read it yourself:

```bash
cat ~/.local/share/rust-connect/api_key
```

Then open KDE Connect on your Android device to pair.

## Install as a service (from source)

Installed the `.deb`? You already have the service — skip this section.
From a source checkout:

```bash
./packaging/install-user-service.sh
```

Builds a release binary into `~/.local/bin`, installs the unit to
`~/.config/systemd/user/`, and enables it. Re-run it to upgrade. Then:

```bash
systemctl --user status rust-connect
journalctl --user -u rust-connect -f
```

This is a **user** service, not a system one, and that is a requirement rather
than a preference. Three parts of the daemon only work inside the desktop
session:

- Desktop notifications go through the session DBus, and the popup path checks
  for `DISPLAY` or `WAYLAND_DISPLAY`.
- The identity and paired-device state live in `~/.local/share/rust-connect`
  (`own.crt`, `own.key`, `device_id`, `paired.json`).
- Received files land in `~/Downloads`.

A system unit with `ProtectHome=yes` cannot see any of that, so it would mint a
fresh identity on every start and silently drop every pairing.

The mousepad and presenter plugins additionally need access to `/dev/uinput`
to inject input. The unit permits the device, but your user still needs
filesystem access to it, via the `input` group or a udev rule. Without it the
rest of the daemon works and remote input does not.

The clipboard plugin syncs the session clipboard for real on Wayland via
`wl-copy`/`wl-paste` (from wl-clipboard): incoming phone content is written
with `wl-copy`, and a persistent `wl-paste --watch` watcher pushes local
changes to connected devices. It needs `WAYLAND_DISPLAY` and
`XDG_RUNTIME_DIR`, which the user unit inherits from the graphical session.
Without a Wayland session or wl-clipboard it degrades to storing clipboard
packets (and serving the REST API) without touching the desktop, and logs the
degradation.

The mpris plugin controls real session media players over D-Bus via zbus:
it discovers `org.mpris.MediaPlayer2.*` players, publishes the player list
and now-playing state to paired devices, and relays the phone's
play/pause/next/previous/stop, seek, and volume commands to the bus. It needs
a session D-Bus (`DBUS_SESSION_BUS_ADDRESS`, inherited from the graphical
session). Without one it degrades to advertising an empty player list, and
logs the degradation.

The runcommand plugin executes shell commands the phone triggers by name,
on an allowlist defined on the desktop. Entries live in the config file
under `[[runcommand.commands]]` and are loaded once at boot — there is
intentionally no runtime write path, so the allowlist can only change by
editing the config and restarting the daemon. Without any entries, the
allowlist stays empty and every request is refused (safe-by-default).
Commands run via `/bin/sh -c` with a 30s timeout and a 64KB output cap,
matching upstream.

Something not working? See [docs/troubleshooting.md](docs/troubleshooting.md)
for firewall, pairing, clipboard, remote input, and certificate fixes.

Tested against real hardware — see [docs/live-validation.md](docs/live-validation.md)
for the feature matrix from live testing against the stock KDE Connect
Android app (pairing with SAS, share both ways, clipboard, notifications,
upgrade continuity).

## CLI Usage

```
rust-connect                    # Daemon with REST API (port 9090)
rust-connect --no-api           # Daemon without the REST API
rust-connect --help             # Full options
```

Subcommands drive the running daemon's REST API instead of starting a
daemon. The API key is read from the data-dir `api_key` file
(`~/.local/share/rust-connect/api_key`) unless `--api-key` /
`RUST_CONNECT_API_KEY` is given; the base URL defaults to
`http://127.0.0.1:9090` unless `--api-url` / `RUST_CONNECT_API_URL` is set.
`--json` prints the raw API envelope for scripting. A key given as `--api-key`
is visible in the process listing (`ps`, `/proc/<pid>/cmdline`), so prefer the
environment variable or the key file on a shared machine.

A key passed as `--api-key` lands in `/proc/<pid>/cmdline`, which any
local user can read, and in your shell history. The key file is the
default for that reason; `RUST_CONNECT_API_KEY` is the next best option.
Reserve the flag for throwaway keys and interactive debugging.

```
rust-connect status             # Daemon health and device count
rust-connect devices            # Table: ID, name, type, state, paired at
rust-connect pair <device-id>   # Outgoing: shows the SAS, waits for the phone.
                                # Incoming request pending: shows the SAS and
                                # asks before accepting (--yes to skip the prompt).
rust-connect unpair <device-id>
rust-connect ping <device-id>
rust-connect share <device-id> <file>
rust-connect clipboard          # Print clipboard (or: clipboard set "text")
```

Exit codes: `0` success, `1` API error, `2` daemon unreachable.

### REST API

```bash
curl -H "X-API-Key: YOUR_KEY" http://localhost:9090/api/v1/devices
```

The REST API is the single control surface. (An MCP server is planned for
v2.)

## API

```
GET    /api/v1/devices                          List paired devices
GET    /api/v1/devices/:device_id               Get device details
POST   /api/v1/devices/:device_id/pair           Pair a device
DELETE /api/v1/devices/:device_id/unpair         Unpair a device
POST   /api/v1/ping                              Send ping
GET    /api/v1/plugins                           List loaded plugins
GET    /api/v1/events?api_key=YOUR_KEY           SSE event stream (text/event-stream)
GET    /api/v1/health                            Liveness, no auth required
GET    /docs                                     Swagger UI (spec: /api-docs/openapi.json)
```

All endpoints except `/api/v1/health` require the API key as an
`X-API-Key` header. The `api_key` query parameter is accepted only on
`/api/v1/events` (browser `EventSource` cannot set headers). Responses
follow `{ status, data, metadata }` format. Errors use structured codes
like `DEVICE_NOT_FOUND`.

The event stream is Server-Sent Events, not a WebSocket. Each event is one
`data:` line carrying a JSON object, so `curl` reads it directly:

```bash
curl -N -H "X-API-Key: YOUR_KEY" http://localhost:9090/api/v1/events
```

`-N` disables curl's output buffering; without it the stream appears to
hang. Device events and plugin events are interleaved on the one stream.

## Web UI

The daemon serves a single-page troubleshooting UI from the binary itself,
no separate build step and no CDN. With the daemon running, open
<http://localhost:9090/> (which redirects to `/ui`).

It is a technical interface, not a polished product surface: it exposes
every device endpoint, every plugin action, and the live event stream, so
that a failure can be localized to the API rather than guessed at. Disable
it with `ui_enabled = false` in the config file; it is on by default.

## Configuration

Settings live in `~/.config/rust-connect/config.toml` (TOML; missing
fields fall back to defaults). The file is loaded once at boot — there
is no live reload.

### `[runcommand]` — desktop-defined command allowlist

Each `[[runcommand.commands]]` table adds one shell command the
runcommand plugin will advertise to paired phones and execute when the
phone sends the matching `key`. The allowlist is **desktop-global**
(visible to every paired device) and is loaded once at boot; absent
section means every command request is refused.

```toml
[[runcommand.commands]]
key = "suspend"
name = "Suspend"
command = "systemctl suspend"

[[runcommand.commands]]
key = "lock"
name = "Lock screen"
command = "loginctl lock-session"
```

- `key` — the lookup the phone sends back (`{"key": "<key>"}`). Must
  be unique across the allowlist.
- `name` — human-readable label shown in the phone's UI.
- `command` — shell snippet executed via `/bin/sh -c`.

Entries with empty `key`, `name`, or `command` are skipped with a
warning at boot (a bad row never fails the daemon). For duplicate keys,
the first entry is kept and the rest are skipped with a warning. There
is no runtime write path — to change the allowlist, edit the file and
restart the daemon.

## Security

- rustls TLS 1.2 with mutual authentication (server role requests the client
  certificate, mirroring Android's SslHelper)
- TOFU certificate pinning (SHA256 fingerprints), enforced during the TLS
  handshake in the custom certificate verifiers
- Short Authentication String (SAS) displayed at pairing, byte-for-byte
  compatible with Android's `PairingHandler.getVerificationKey`
- Payload (file) transfers over TLS, bounded to the declared `payloadSize`
- Auto-generated UUID API key on first run
- Input validation and pairing rate limiting (max 10 concurrent pending)

### Fuzzing

The wire parser (`PacketSerializer::deserialize`), the UDP identity
decode path, and the multipart share upload path are fuzzed with
cargo-fuzz (libFuzzer). The fuzz crate lives in
`fuzz/` (requires nightly, pinned there via `rust-toolchain.toml`):

```
cargo install cargo-fuzz
cd fuzz
cargo fuzz run packet_deserialize   # raw bytes -> deserialize (+ round-trip)
cargo fuzz run identity_packet      # discovery identity-packet decode path
cargo fuzz run share_multipart      # multipart share upload parsing
```

Seed inputs (valid identity/pair/ping/share packets plus boundary cases:
empty, 512 KiB limit, truncated JSON, deep nesting) live in
`fuzz/corpus/<target>/`; any crashes are written to `fuzz/artifacts/`.
CI runs a 60s smoke pass per target on PRs touching `src/protocol/` and a
weekly 10-minute run (`.github/workflows/fuzz.yml`).

## Project Structure

```
src/
  main.rs            Entry point
  lib.rs             Library root (the crate builds as both lib and bin)
  bootstrap.rs       Production wiring (AppState construction, desktop backends)
  daemon.rs          Service orchestration, reconnect, signal handling
  app.rs             Shared state (AppState)
  api/               REST API + SSE (router, handlers, auth, middleware, sse, openapi, ui)
  cli/               clap CLI and the client-mode subcommands
  device/            Device registry, lifecycle, event broadcasting
  protocol/          KDE Connect protocol (discovery, connection, pairing, packet, router, crypto, connection_loop, listener, payload_transfer)
  plugins/           Plugin system, 25 plugins (ping, battery, notification, sms, clipboard, share, mpris, telephony, pausemusic, connectivity, sftp, mousepad, lock, systemvolume, findmyphone, findthisdevice, presenter, contacts, runcommand, sendnotifications, remotekeyboard, digitizer, screensaver_inhibit, remotecommands, shareinputdevices)
  services/          Service manager and connection orchestration
  config/            Settings
  utils/             Errors, logging
tests/               Integration tests (API, protocol, CLI)
docs/reference/      Android upstream sources the implementation conforms to
docs/archive/        Historical planning documents
```

## Development

```bash
cargo test        # Run tests
cargo fmt         # Format
cargo clippy      # Lint
```

Live-device integration tests (require an Android device on adb):

```bash
RUST_CONNECT_TEST_USB=1 cargo test --test usb_integration -- --ignored --nocapture
```

## License

GPL-2.0-or-later (compatible with KDE Connect)
