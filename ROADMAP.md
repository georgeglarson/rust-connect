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

## Functional completeness

The canonical implementation plan is
[`docs/functional-completeness-plan.md`](docs/functional-completeness-plan.md).
It replaces the earlier assumption that the nine behavioral gaps in
`docs/parity-checklist.md` were the whole remaining surface. That checklist
explicitly excluded plugin-level parity, and live validation covers only a
subset of advertised features and environments.

The immediate order is:

1. build an exhaustive capability/evidence ledger from current Android, KDE,
   GSConnect, and production Rust wiring;
2. close advertised-but-incomplete desktop effects (system volume,
   run-command configuration, SFTP browsing, notification actions/icons,
   MPRIS album art, and smaller backend gaps);
3. close the nine known protocol gaps and recovery/security fault cases;
4. classify and implement the remaining upstream feature union;
5. validate A15 and S21 plus Sway, GNOME, KDE, Wayland, and X11; and
6. make inventory/evidence closure a release gate.

Completeness is now a bounded claim. `UNVERIFIED` remains visible until a real
peer, upstream-derived fixture, or applicable environment proves the behavior.

## Next

- Sprint 3 of the functional-completeness plan: audit and implement the
  remaining upstream feature union (remotecontrol, shareinputdevices,
  virtualmonitor), and build the kdeconnectd independent-peer interop
  harness that announcement claims require.
- Then Sprint 4 (device/desktop/soak matrices) and Sprint 5 (evidence
  closure as a release gate). Sprints 0–2 are complete: the ledger exists
  and is lint-enforced, advertised features have their desktop effects,
  and the protocol/security fault cases are closed.

## Later

- Desktop shell integration: tray/indicator and native pairing UI.
- Packaging beyond the deb script: rpm, then Flatpak.
- Broader distro matrix (non-systemd, musl, immutable distros).
- macOS exploration. The protocol is portable; the desktop integration
  is the open question.

## Not planned

- Windows support. The session integration model (D-Bus, systemd user
  units, Wayland) does not translate, and kdeconnect-kde already serves
  that platform.
- Telemetry of any kind. No crash reporting, no usage metrics, no
  phone-home. Bugs get reported by users, in issues.
