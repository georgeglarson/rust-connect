# Contributing to rust-connect

Bug reports, protocol findings, and patches are all welcome. This page
covers what you need to build the project and what a mergeable change
looks like.

## Getting set up

Requirements:

- Rust 1.91 or newer. Install via [rustup](https://rustup.rs/). The MSRV is
  declared as `rust-version` in `Cargo.toml` and checked in CI.
- `pkg-config`, `libdbus-1-dev`, and `libssl-dev`. D-Bus is needed by the
  MPRIS and notification plugins; OpenSSL is a dev-dependency only, used to
  build certificate fixtures for the test suite. The production binary does
  not link OpenSSL.
- An Android device running the KDE Connect app, if you want to exercise
  the live-device tests. Everything else runs without hardware.

On Debian or Ubuntu:

```bash
sudo apt-get install -y pkg-config libdbus-1-dev libssl-dev
```

Then:

```bash
git clone https://github.com/georgeglarson/rust-connect
cd rust-connect
rustup component add rustfmt clippy
cargo build --locked
cargo test --all-features --locked
```

Run the daemon in the foreground while you work on it:

```bash
cargo run
```

It generates an API key on first run and logs it. `RUST_LOG=debug cargo run`
turns up the volume.

## The four gates

Every change has to clear these before it can merge. CI runs all four, so
running them locally saves a round trip:

```bash
cargo build --locked                          # production features only
cargo test --all-features --locked
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

`cargo build --locked` deliberately omits `--all-features`. The crate carries
a dev-dependency on itself with the `test-helpers` feature enabled, which
unifies that feature into every `cargo test` build. A production-only compile
break can therefore pass the test gate while breaking the shipped binary, so
the plain build runs as its own gate.

The `test-helpers` feature exists to expose constructors and fixtures that
tests need and production does not. If you add one, gate it behind
`#[cfg(feature = "test-helpers")]` rather than making it unconditionally
public.

## Where things live

Read `src/` top-level first; the module names carry the layering.

| Path | What it owns |
|---|---|
| `src/main.rs`, `src/lib.rs` | Binary entry point; library root. The crate builds as both. |
| `src/bootstrap.rs` | Production wiring. Constructs `AppState` and enables the desktop backends (Wayland clipboard, session D-Bus). Tests never go through here, which is how the plugins stay testable without a desktop. |
| `src/daemon.rs` | Service orchestration, reconnect, signal handling. |
| `src/app.rs` | `AppState`, the shared handle everything else takes. |
| `src/protocol/` | The KDE Connect wire protocol: `discovery.rs` (UDP), `listener.rs` and `connection/` (TCP + TLS), `pairing/`, `crypto.rs` (certificates, TOFU pinning, SAS), `packet.rs`, `router.rs`, `payload_transfer.rs`. |
| `src/device/` | Device registry, lifecycle, event broadcasting. |
| `src/plugins/` | The 24 plugins, plus `loader.rs` (registration) and `registry.rs` (dispatch). |
| `src/api/` | axum REST API: `router.rs` (routes), `handlers/` (request processing), `auth.rs`, `middleware.rs`, `sse.rs` (the event stream), `openapi.rs`, `ui/` (the embedded troubleshooting page). |
| `src/cli/` | clap definitions and the client-mode subcommands that drive a running daemon over its own REST API. |
| `src/config/`, `src/services/`, `src/utils/` | Settings; service manager and connection orchestration; errors and logging. |
| `tests/` | Integration tests. `usb_integration.rs` needs a real phone and is `#[ignore]`d by default. |
| `fuzz/` | A separate crate holding the cargo-fuzz targets and their corpus. Requires nightly, pinned by its own `rust-toolchain.toml`. |
| `docs/reference/` | Upstream Android and KDE sources the implementation conforms to. When a behavior looks arbitrary, the answer is usually in here. |

## Protocol changes need a citation

The single hardest constraint on this project is that the Android app is the
oracle and it is not going to change for us. Any patch that alters
on-the-wire behavior should cite the upstream file and line it is matching,
the way the existing code and `KDECONNECT_PROTOCOL.md` do. "This seems more
correct" is not enough; "kdeconnect-android `PairingHandler.kt:112-118` does
it this way" is.

## Branches and commits

Branch names use one of three prefixes, then a short hyphenated topic:

```
fix-<topic>      bug fix
feat-<topic>     new feature
chore-<topic>    refactor, dependency bump, cleanup
```

Commit messages use the conventional-commit prefixes (`fix:`, `feat:`,
`docs:`, `test:`, `refactor:`, `perf:`, `chore:`), optionally scoped:
`fix(pairing): refuse pairing state for our own device id`.

Keep commits small enough to read. Squash-merge is the default, so a messy
local history is fine as long as the branch as a whole is coherent.

## Tests

New behavior comes with a test that fails without it. Unit tests live in a
`#[cfg(test)] mod tests` block in the same file; cross-module and API tests
live in `tests/`.

Two things to watch:

- **Do not let a test touch the real desktop.** The clipboard, MPRIS,
  notification, and screensaver plugins all have their session backends
  enabled only in `bootstrap.rs`. A test that constructs a plugin directly
  gets the inert version, and that is deliberate.
- **Live-device tests are opt-in.** They only run with the environment
  variable set and the `--ignored` flag:

  ```bash
  RUST_CONNECT_TEST_USB=1 cargo test --test usb_integration -- --ignored --nocapture
  ```

## Fuzzing

The packet deserializer and the UDP identity decode path have cargo-fuzz
targets. The fuzz crate is separate and needs nightly:

```bash
cargo install cargo-fuzz
cd fuzz
cargo fuzz run packet_deserialize
cargo fuzz run identity_packet
```

CI runs a 60-second smoke pass per target on any PR touching
`src/protocol/`, plus a weekly ten-minute run. If you find a crash, the
reproducer lands in `fuzz/artifacts/`; attach it to the issue.

## Pull requests

Open the PR against `main`. Fill in the template. CI runs the four gates
plus `cargo audit` and `cargo deny`; automated reviewers comment on the
diff. Treat their findings as hypotheses rather than instructions: some are
right, some have not read enough context. Reply inline when one is wrong,
push a fixup when it is right.

A change that alters user-visible behavior should update `README.md` in the
same PR, and add a line to `CHANGELOG.md` under `[Unreleased]`.

## Getting help

Open a GitHub issue. Bug reports and feature requests both have templates.
Protocol questions are welcome as issues too; the answer often improves
`KDECONNECT_PROTOCOL.md` for the next person.

## License

By contributing, you agree that your contributions are licensed under
GPL-2.0-or-later, the same license as the project.
