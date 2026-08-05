# Roadmap

This is the current state of rust-connect and where it is going. It is a
planning document, not a promise: order and scope change as reality
demands. To influence prioritization, open an issue describing what you
need and why; concrete use cases carry more weight than votes.

## Where we are

rust-connect 0.1.0 is a working daemon with a REST API, compatible with
the existing KDE Connect Android app.

- KDE Connect protocol on the LAN: UDP and mDNS discovery, TCP transport,
  TLS 1.2 with TOFU pinning, SAS pairing, byte-compatible with the Android
  app.
- 24 plugins wired end-to-end: ping, battery, notification, sms,
  clipboard, share, mpris, telephony, pausemusic, connectivity, sftp,
  mousepad, lock, systemvolume, findmyphone, findthisdevice, presenter,
  contacts, runcommand, sendnotifications, remotekeyboard, digitizer,
  screensaver-inhibit, remotecommands.
- REST API (`/api/v1/`) as the single control surface, with an SSE event
  stream at `/api/v1/events`; the primary consumers are automated agents.
- A CLI client on top of that API (`status`, `devices`, `pair`, `unpair`,
  `ping`, `share`, `clipboard`), so the API is not curl-only.
- An embedded troubleshooting web UI served from the binary at `/ui`.
- cargo-fuzz targets over the packet deserializer and the UDP identity
  decode path, with a seeded corpus and a CI smoke pass on every PR that
  touches `src/protocol/`.
- Linux desktop only. systemd user unit, session D-Bus, Wayland/X11
  integration where the plugins need it.

Interop focus is Android; kdeconnect-kde and GSConnect peers work but get
less validation.

## Next

- Live-device validation matrix: every plugin against real Android
  devices across app versions, tracked and repeatable.
- Notification icons and inline actions. Notifications sync today, but
  the icon payload and the action buttons the Android app offers are not
  wired through.
- systemvolume as a real provider: the plugin answers volume requests,
  but the desktop side does not yet publish sink lists and per-sink
  volume from PipeWire/PulseAudio.
- sftp as an actual mount rather than an advertised endpoint, so browsing
  the phone works from a file manager.
- Packaging beyond the deb script: rpm, then Flatpak.

## Later

- Desktop shell integration: tray/indicator and native pairing UI.
- Absolute pointer positioning for `kdeconnect.mousepad.request` packets
  carrying `x`/`y`. These need absolute axes on the uinput device, which
  the current relative-only pointer does not register. No shipped client
  sends them over the network today, so they are logged and dropped.
- Broader distro matrix (non-systemd, musl, immutable distros).
- macOS exploration. The protocol is portable; the desktop integration
  is the open question.

## Not planned

- Windows support. The session integration model (D-Bus, systemd user
  units, Wayland) does not translate, and kdeconnect-kde already serves
  that platform.
- Telemetry of any kind. No crash reporting, no usage metrics, no
  phone-home. Bugs get reported by users, in issues.
