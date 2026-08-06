# Troubleshooting

First-run and common issues, observed during live validation. Symptom → cause
→ fix. Defaults: data dir `~/.local/share/rust-connect`, config
`~/.config/rust-connect/config.toml`, API on `127.0.0.1:9090`.

## Phone and computer never see each other

Almost always the firewall: discovery (UDP broadcast) and the link (TCP)
both need ports 1716–1764.

```bash
# ufw
sudo ufw allow 1716:1764/udp
sudo ufw allow 1716:1764/tcp

# firewalld
sudo firewall-cmd --permanent --add-port=1716-1764/udp --add-port=1716-1764/tcp
sudo firewall-cmd --reload
```

Both devices must be on the same network — see the last section.

## Pairing request "never arrives" on the phone

It did — as a **silent notification** in the Android notification shade, not a
pop-up. Pull down the shade and look for the KDE Connect notification with
Accept/Reject actions.

Pairing windows are short: 25 s incoming, 30 s outgoing. If one expires, just
request again — nothing is broken.

While a request is pending, the verification key (SAS) is shown in the
phone's pairing dialog and via the API; the two must match before you accept:

```bash
curl -H "X-API-Key: $(cat ~/.local/share/rust-connect/api_key)" \
    http://127.0.0.1:9090/api/v1/devices/DEVICE_ID
# -> "verification_key": "AB12CD34" (present only while pairing is pending)
```

## Phone→desktop clipboard doesn't sync (Android 10+)

Android 10+ blocks background apps from reading the clipboard — an OS
restriction, not a rust-connect bug. Copy while the KDE Connect app is in
the foreground, or use the app's send-clipboard tile. Desktop→phone works at
any time.

## Remote input (mousepad/presenter) does nothing

The daemon can't open `/dev/uinput`. The user unit already permits it
(`DeviceAllow=/dev/uinput rw`), but your user still needs filesystem access:

```bash
sudo modprobe uinput
echo uinput | sudo tee /etc/modules-load.d/uinput.conf   # persist across boots
echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/99-uinput.rules
sudo udevadm control --reload-rules && sudo udevadm trigger /dev/uinput
sudo usermod -aG input "$USER"
```

Group membership only applies to new sessions: **log out and back in**, then
`systemctl --user restart rust-connect`. Everything else works without this; only
input injection fails.

## Clipboard sync does nothing on the desktop

On Wayland the plugin needs `wl-copy`/`wl-paste` (the `wl-clipboard` package)
and a graphical session (`WAYLAND_DISPLAY` + `XDG_RUNTIME_DIR`, inherited by the
user service). Install it (`apt install wl-clipboard`, `pacman -S wl-clipboard`, ...).

X11 has **no** clipboard backend yet (no xclip/xsel support), and headless
sessions have no clipboard at all. In both cases the plugin degrades gracefully:
it still stores clipboard packets from the phone and serves them over
`GET /api/v1/clipboard`, it just never touches the desktop clipboard. The
degradation is logged at startup.

## API returns 401 / how to authenticate

The API key is an auto-generated UUID, stored owner-only (0600) at
`~/.local/share/rust-connect/api_key`:

```bash
KEY=$(cat ~/.local/share/rust-connect/api_key)
curl -H "X-API-Key: $KEY" http://127.0.0.1:9090/api/v1/devices
```

For the SSE event stream (`GET /api/v1/events`) — where browser
`EventSource` can't set headers — `?api_key=$KEY` works too.

The API binds `127.0.0.1` by default. Setting `api_bind = "0.0.0.0"` in
config.toml exposes it on the LAN, which turns it into a remote control surface
for your desktop (clipboard, notifications, input injection, file transfer):
anyone who reaches port 9090 with the key has full control. Only do this on a
network you trust, and treat the key as a secret.

## Where are the logs?

```bash
journalctl --user -u rust-connect -f                 # systemd journal
ls ~/.local/share/rust-connect/daemon.*.log          # rotating daemon logs
```

File logs rotate hourly, 24 files kept (`log_max_files` in config.toml).
Use `RUST_LOG=debug` in the unit or `log_level = "debug"` for more detail.

## Service doesn't start on boot / dies at logout

The installer runs `systemctl --user enable --now rust-connect`, but a user
service only runs while you're logged in:

```bash
systemctl --user enable --now rust-connect   # not enabled at all
sudo loginctl enable-linger "$USER"          # headless: run without a session
```

Without linger, systemd stops the service when your last session ends.

## Certificate problems

Certificates live in `~/.local/share/rust-connect/certs/`: your identity is
`own.crt` + `own.key`, each paired device is `<device_id>_peer.crt`.

- **Expiry**: your own certificate is valid from 1 year before issuance to 10
  years after (matching Android). An expired one is regenerated automatically
  at startup — no action needed.
- **Fingerprint mismatch on connect**: the peer presented a different
  certificate than the pinned one (TOFU) — usually the device was reinstalled
  or reflashed and minted a new identity. If you expected that, reset the trust
  and pair again:

```bash
curl -X DELETE -H "X-API-Key: $(cat ~/.local/share/rust-connect/api_key)" \
    http://127.0.0.1:9090/api/v1/devices/DEVICE_ID/unpair
rm ~/.local/share/rust-connect/certs/DEVICE_ID_peer.crt
systemctl --user restart rust-connect
```

If you did **not** expect the identity to change, don't re-pair — an
unexplained mismatch is exactly what the pinning exists to catch.

## Discovery can't work on this network

Devices never appear even with the firewall open: the network blocks broadcast
or client-to-client traffic — client-isolated Wi-Fi (guest nets, campus APs),
VPNs that capture all traffic, or devices on different VLANs/subnets.

If plain host-to-host traffic is allowed, connect manually over TCP. You need
the phone's IP (shown in the KDE Connect app) and the device must already be
known to the daemon (previously paired or seen):

```bash
curl -X POST -H "X-API-Key: $(cat ~/.local/share/rust-connect/api_key)" \
    -H "Content-Type: application/json" \
    -d '{"address": "192.168.1.50:1716"}' \
    http://127.0.0.1:9090/api/v1/devices/DEVICE_ID/connect
```

This dials the device directly and runs the normal TLS/pairing flow — as secure
as a discovered connection. If client isolation blocks direct connections too,
there is no workaround on that network; move both devices to the same
non-isolated network.

## SFTP browsing — `browse_sftp` is unavailable or the mount fails

The SFTP mount is the only feature that needs a **local** tool beyond the
daemon itself: it spawns `sshfs` (and unmounts via `fusermount3` /
`fusermount`). The daemon reports backend availability honestly — when
either is missing, `/api/v1/tools` lists `browse_sftp` with
`available: false` and the `POST /devices/{id}/sftp/mount` endpoint
returns HTTP 503.

Install the prerequisite packages:

```bash
# Debian / Ubuntu
sudo apt install sshfs fuse3

# Fedora
sudo dnf install fuse-sshfs fuse3

# Arch
sudo pacman -S sshfs fuse3
```

After installing, log out and back in (FUSE needs the `fuse` group on
the user), then check:

```bash
which sshfs fusermount3          # both must be found
curl -H "X-API-Key: $(cat ~/.local/share/rust-connect/api_key)" \
    http://127.0.0.1:9090/api/v1/tools | jq '.data.tools[] | select(.name=="browse_sftp")'
# -> "available": true
```

### Where the mount appears and why it disappears

The mount point is **server-determined** and lives under your data dir:

```
~/.local/share/rust-connect/mounts/sftp-<device_id>/
```

The desktop does not let you pick the path — caller-controlled paths
were an XSS-style surface in the old contract and are gone. The
mount is released automatically on:

- device disconnect (`on_disconnected`)
- unpair (`DELETE /devices/{id}/unpair`)
- device deletion (`DELETE /devices/{id}`)
- daemon shutdown (after `stop_services` runs, so no new mount
  requests race in)
- daemon startup — any stale mount left by a previous crash is
  released by `startup_sweep` before the API server starts accepting
  requests

If a mount is left in place after a crash, the next boot will clean it
up; you don't need to do anything.

### Credentials are never persisted

`kdeconnect.sftp` packets carry the SFTP password in memory only.
After a daemon restart the desktop has no way to mount anything until
the phone re-sends the credentials — the **deliberate** behavior. The
phone's Android app sends a fresh `kdeconnect.sftp` packet every time
the SFTP browsing session is requested, so a re-request from the UI is
enough to restore access. The password never appears in:

- process argv (sshfs is invoked with `-o password_stdin` and the
  password travels on stdin; see the `mounter.rs` doc comment + the
  upstream citation)
- env vars (the runner never sets the password as an env var)
- the API response (the `get_sftp_info` handler omits the field; a
  regression test pins this)
- log lines (a custom `Debug` on `SftpConnectionInfo` redacts the
  password as `***redacted***`; the `sftp_connected` log line does not
  carry the field)
- the filesystem (no `password` file is written; mounts dir is
  created on demand and the password is dropped on every cleanup leg)

### Mount failures show up in the response, not just the log

A failed mount returns HTTP 200 with `mount_state: "failed"` and the
sshfs stderr in the `error` field of the response body, e.g.:

```json
{
  "status": "ok",
  "data": {
    "device_id": "...",
    "mounted": false,
    "mount_state": "failed",
    "mount_point": "/home/.../mounts/sftp-...",
    "error": "remote host has changed"
  }
}
```

This is a deliberate departure from "log and 500" — the caller (the
web UI, an MCP agent) needs the reason to surface a useful message
without parsing daemon logs.
