## Description

<!-- What this changes and why. Link the issue if one exists. -->

## Checklist

- [ ] `cargo test --locked` passes locally
- [ ] `cargo clippy -- -D warnings` is clean
- [ ] `cargo fmt --check` is clean
- [ ] `cargo deny check` run (required if `Cargo.toml` / `Cargo.lock` changed)

## Protocol compatibility

<!-- Does this change wire behavior relative to the Android KDE Connect app
     (packet types, fields, TLS, pairing, discovery, payload transfers)?
     State "no wire impact" explicitly if not. If yes, describe the change
     and how it was validated against the Android app or upstream source. -->

## Security impact

<!-- State "none" explicitly if none. Otherwise: does this touch
     authentication, pairing, TLS, the REST API auth path, file writes,
     input injection, or new network input? Read docs/threat-model.md
     first and identify the affected adversary class. -->
