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
