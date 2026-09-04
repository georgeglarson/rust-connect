# FINDINGS — chore-audit-doc-drift

Date: 2026-09-04. Executor: M3 lane. Branch: `chore-audit-doc-drift`.
Source brief: vault `projects/rust-connect-audit-2026-09-02.md` "Docs drift" tail
(remaining items after the 2026-09-02 docs batch, `85e213e`). Merge class: A.

## What changed

Six items from the brief, each in its own commit (commits not pushed — the
integrator verifies):

| # | Item | Files | Commit message |
|---|---|---|---|
| 1 | Plugin count drift (24 → 25) | `CONTRIBUTING.md`, `KDECONNECT_PROTOCOL.md` | `docs: fix stale plugin count (24 -> 25)` |
| 2 | ROADMAP "Next" still lists shipped work | `ROADMAP.md` | `ROADMAP: stop listing remotecontrol/shareinputdevices as upcoming` |
| 3 | SECURITY.md misdescribes the hardening | `SECURITY.md` | `SECURITY.md: rewrite sandbox section to match the unit` |
| 4 | Tracked build artifacts | (no change) | already removed by `b3db5ec`; .gitignore entries present |
| 5 | release.yml has no test gate on the tag | `.github/workflows/release.yml` | `release.yml: --no-fail-fast on the gate's test step` |
| 6 | device.rs splits past 900-line anti-example | `src/api/handlers/device.rs`, `src/api/handlers/device_tests.rs`, `src/api/handlers/mod.rs` | `device.rs: split the test module into a sibling file` |

### 1. Plugin count drift

Sweep evidence — every "24 plugin" string in the tree before:

```
CONTRIBUTING.md:84: | `src/plugins/` | The 24 plugins, plus `loader.rs` (registration) and `registry.rs` (dispatch). |
KDECONNECT_PROTOCOL.md:171: **Supported packet types** (the 24 plugins registered in `src/plugins/loader.rs`; several share a packet type, so the roster is longer than this list):
KDECONNECT_PROTOCOL.md:204: **The full 24-plugin roster** (the registration order, which is the order
KDECONNECT_PROTOCOL.md:212: …, remotecommands.
```

After: 0 occurrences of `\b24 plugins?\b` or `\b24-plugin\b` in `*.md`.

Sweep also rechecked README/ROADMAP/CHANGELOG. They already said "25 plugins"
(verified via `grep -n 'plugin' README.md ROADMAP.md CHANGELOG.md`). No
spurious fix needed; only CONTRIBUTING and KDECONNECT_PROTOCOL were stale.

The KDECONNECT_PROTOCOL roster on line 204 listed 24 plugin names and was
missing `shareinputdevices` at the end (the only one added since the roster
was last updated). Added it as the final bullet so the roster's claim of
"the full 25-plugin roster" is true.

Plugin count source of truth: `PluginAccess` in `src/plugins/mod.rs` — 25
fields, confirmed with `grep -E "^\s+(pub )?[a-z_]+:" src/plugins/mod.rs | wc -l`
→ 25. No new count source invented.

### 2. ROADMAP "Next" was stale

Before:
```
## Next
- Sprint 3 of the functional-completeness plan: remotecontrol and
  shareinputdevices landed (2026-08-23 and 08-26); virtualmonitor is the
  remaining upstream feature. The kdeconnectd independent-peer interop
  harness (`tests/interop/`) exists and is source-pinned.
```

The ROADMAP was listing `remotecontrol` and `shareinputdevices` as items
in the "Next" section even though both had shipped (CHANGELOG 0.1.0 already
records them in the "Added" list of the initial feature set, and the file's
own "Where we are" section at lines 16-22 enumerates both as wired). The
brief asked to "move them to the done/shipped section or strike them,
matching how the file records shipped work" — the file already records them
in "Where we are", so the right move was to strike the redundant mention
from "Next". `virtualmonitor` stays (it's the actual remaining upstream
feature, see `docs/functional-completeness-plan.md`).

### 3. SECURITY.md sandbox section

Before, the listing under "Mitigations that do exist" enumerated the
hardening directives set by the unit and tacked on a single sentence:
> `NoNewPrivileges` is deliberately OFF: SFTP mounts go through the
> setuid `fusermount3`, which `NoNewPrivileges=yes` would block.

After, the same section describes the actual unit posture as it stands at
HEAD (`packaging/rust-connect.service`):

- lists the directives the unit actually sets, including the missing
  `SystemCallArchitectures=native` and `ConditionUser=!gdm-greeter`
  (the latter added in commit `8ff7d25`, audit 2026-09-02 §E);
- gives the real reason `NoNewPrivileges=no` and `RestrictSUIDSGID=no`:
  unprivileged users can't `mount()` (needs `CAP_SYS_ADMIN`), so sshfs
  must delegate to the setuid `fusermount3` helper, which requires
  privilege elevation to drop privileges. With either flag on,
  `fusermount3` core-dumps in `drop_privs` (observed live 2026-08-06);
- names the maintenance contract: after a systemd upgrade, re-diff
  `@privileged` against the `SystemCallFilter` carve-out in
  `packaging/rust-connect.service`, drift silently breaks SFTP mounts
  (EPERM/SIGSYS) or widens the sandbox.

The brief's own framing ("input-method buses like ibus need privilege
escalation over X11 sockets") didn't match the unit's actual comment —
the unit's reason is fusermount3/SFTP, not ibus/X11. I followed what the
unit says, not the brief's misremembered rationale. The doc now agrees
with `packaging/rust-connect.service:37-44`.

### 4. Tracked build artifacts — already done

`git rm` of `packaging/deb/usr/bin/rust-connect` and the ~6.8 MB
`rust-connect_*.deb` was already done by commit `b3db5ec` ("docs, packaging,
release: audit hygiene items"). `.gitignore` already carries
`/packaging/deb/usr/` and `/packaging/*.deb`. `git ls-files packaging/` shows
no binary, no .deb — only the build inputs (DEBIAN scripts, the source
`rust-connect.service` template, `build-deb.sh`, `install-user-service.sh`).
The `packaging/deb/usr/lib/systemd/user/rust-connect.service` is tracked
because the build copies it from `packaging/rust-connect.service` at
`build-deb.sh:57` and writes it back each run; this is a build output but
not the binary the brief called out. Left as-is.

### 5. release.yml test gate

The release workflow already had a `gate` job that runs `cargo test`,
`cargo clippy`, `cargo fmt --check`, and the `build-and-release` job had
`needs: gate` (added in commit `b3db5ec`). The brief's specific ask was
`cargo test --all-features --locked --no-fail-fast` (the `--no-fail-fast`
makes the run surface every test instead of bailing at the first failure —
matters when the gate fails and someone wants the whole picture without
re-running each suite). Added the flag; everything else already matched
the brief.

YAML parses: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"` → OK.

### 6. device.rs split

The constitution's 900-line anti-example in `docs/constitution.md` lines
45-47 says: "Handler files at 900+ lines (21 handlers each repeating auth
boilerplate)". `src/api/handlers/device.rs` was at 1221 lines (the brief
said "~1112" but the actual count at HEAD was 1221) with 11 handlers and
no auth-boilerplate repetition — the file doesn't fit the anti-example's
mechanics, but it still violates the line threshold the constitution
calls out.

**Seam analysis.** The file's structure, top to bottom:

| Lines | Section |
|---|---|
| 1-14 | module doc + `use` imports |
| 15-36 | `reconcile_rendered_connection_state` helper |
| 38-105 | `list_devices` (records read) |
| 101-153 | `get_device` (records read) |
| 155-282 | `pair_device` (largest handler, 127 lines, accept/initiate branches) |
| 284-348 | `unpair_device` |
| 350-371 | `notify_peer_unpair` helper |
| 373-417 | `send_ping` |
| 420-468 | `delete_device` |
| 471-523 | `connect_device` |
| 526-557 | `disconnect_device` |
| 560-587 | `get_device_state` |
| 589-616 | `list_connected_devices` |
| 618-1221 | `#[cfg(test)] mod tests { … }` (604 lines, 10 tests + 5 helpers + `UnpairRecorderPlugin` mock) |

There is no records/serialization half. `DeviceListResponse` and
`DeviceResponse` are constructed inline in `list_devices`/`get_device`
using `serde_json::json!()` — no separate type-construction code to
extract. `list_devices` and `get_device` together are ~105 lines, which
doesn't bring `device.rs` under 900 if moved alone.

The pair/unpair family (`pair_device` + `unpair_device` +
`notify_peer_unpair`, ~214 lines with their `#[utoipa::path]` decorators)
is the only semantically-coherent production seam. After moving it,
`device.rs` is at 1007 lines — still over 900. Moving it alone doesn't
satisfy the brief.

The test module is the file's cleanest mechanical seam and it brings
`device.rs` from 1221 → 616 lines (under both the 500-line target and
the 900-line anti-example). The codebase already uses this pattern in
two places:

- `src/protocol/pairing/mod.rs` + `src/protocol/pairing/tests.rs`
- `src/protocol/connection/mod.rs` + `src/protocol/connection/tests.rs`

Both declare `#[cfg(test)] mod tests;` at the bottom of the production
file. Following that convention, the split:

- creates `src/api/handlers/device_tests.rs` as a sibling of `device.rs`,
  not a child — the device handlers ship from `mod.rs` via `pub use
  device::*` and re-exporting `device_tests::*` would expose test-only
  symbols into the public surface. Sibling is cleaner;
- removes the entire `#[cfg(test)] mod tests { … }` block from `device.rs`
  (lines 618-1221);
- adds `#[cfg(test)] mod device_tests;` to `src/api/handlers/mod.rs`;
- rewrites the test imports: `use super::*;` → `use crate::api::handlers::device::*;`,
  plus explicit `use` for `Arc`, `Path`, `State`, `AppState`, `DeviceState`,
  `CertificateManager`, `ConnectionManager`, `AppSettings` — these came
  through `super::*` in the original child-module layout because
  `device.rs`'s own `use` block re-exported them transitively.

Behavior: byte-identical test bodies, byte-identical assertions, same
mock plugin (`UnpairRecorderPlugin`), same test helpers (`test_state`,
`connect_peer`, `pair_locally`, `make_test_peer_cert_der`).

Public API: `mod device_tests;` is `#[cfg(test)]`, so it doesn't
participate in release builds. No symbol from `device.rs` was renamed
or removed. `pub use device::*` in `mod.rs` still re-exports every
handler.

After `cargo fmt`: rustfmt rewrapped `Arc::new(ConnectionManager::new(...))`
and a long `is_some_and(...)` chain. Both are mechanical wraps, no
semantic change. The line count delta is `device.rs`: 1221 → 616,
`device_tests.rs`: 0 → 619.

## How it was verified

Real run output (commands run from `/tmp/delegate-rust-connect-chore-audit-doc-drift`,
target dir pinned to `/home/glitchenstein/repos/rust-connect/target` because
the worktree is on tmpfs):

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --all-features --locked --no-fail-fast
…
test result: ok. 1157 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.61s
…
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.79s
   (api::handlers::device_tests, the 10 device-handler tests now at the new path)

$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo clippy --all-targets --all-features --locked -- -D warnings
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 24.56s
   (zero warnings)

$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo fmt --check
   (no output — clean)

$ python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"
   YAML parses OK

$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo build --release --locked
   Finished `release` profile [optimized] target(s) in 1m 56s
```

Per-test confirmation that the device-handler tests now live at
`api::handlers::device_tests::*` and not `api::handlers::device::tests::*`:

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
  cargo test --all-features --locked --no-fail-fast --lib api::handlers::device_tests
…
test api::handlers::device_tests::test_list_devices_renders_live_link_as_connected ... ok
test api::handlers::device_tests::test_get_device_renders_dead_link_as_disconnected ... ok
test api::handlers::device_tests::test_pair_accept_sends_response_then_marks_paired ... ok
test api::handlers::device_tests::test_pair_accept_sends_plugin_init_packets ... ok
test api::handlers::device_tests::test_pair_initiate_surfaces_verification_key ... ok
test api::handlers::device_tests::test_pair_accept_unreachable_peer_does_not_mark_paired ... ok
test api::handlers::device_tests::test_unpair_connected_peer_sends_pair_false ... ok
test api::handlers::device_tests::test_unpair_unreachable_device_still_succeeds ... ok
test api::handlers::device_tests::test_unpair_teardown_runs_even_while_connected ... ok
test api::handlers::device_tests::test_disconnect_device_after_replacement_tears_down_current_link ... ok

test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 1147 filtered out; finished in 0.79s
```

The split is the change's gate, and it holds: every test that ran under
`api::handlers::device::tests` runs identically under
`api::handlers::device_tests`. The 1147-filtered set is the rest of the
lib (auth, extractors, share, plugins/*, protocol/*, etc.) — unchanged.

`tests/clipboard_x11.rs` is in the full-suite pass; no `TMPDIR` override
was set, per the brief. `tests/interop/run.sh` was not run (per the brief).

No target package cache was used — `target/package/` was removed before
`cargo check` after the device.rs split, to avoid a stale package cache
shadowing the new file layout.

## Critique — blunt

The brief has three real problems the integrator should know about:

1. **The brief's reasoning for `SECURITY.md` is wrong, but the underlying
   drift is real.** The brief says "the unit's own comment explains why
   (input-method buses like ibus need privilege escalation over X11
   sockets)". That is not what `packaging/rust-connect.service` says.
   The unit's comment (lines 37-44) is about SFTP/fusermount3 and
   `CAP_SYS_ADMIN` — nothing about ibus. I rewrote the doc against what
   the unit actually says, not against the brief's misremembered
   rationale. The change still lands the right idea (NoNewPrivileges is
   off, the unit's comment is the source of truth), but the specific
   framing the brief asked for would have introduced a new drift. If the
   brief's author has a different unit in mind, they should check which
   box they were looking at.

2. **The brief assumes `device.rs` has a "records/serialization half".
   It doesn't.** `list_devices` constructs a `DeviceListResponse` and
   `get_device` returns a `Device`, but those are constructed inline
   with `serde_json::json!()` — there's no separate type-construction
   code to extract. The pair/unpair family is the only coherent
   production seam, and moving it alone leaves `device.rs` at 1007 lines
   (still over 900). The split that actually satisfies the line
   threshold is the test module — which is mechanical, byte-identical,
   and follows the codebase's existing convention from `protocol/pairing`
   and `protocol/connection`. The brief's example filename
   `device_records.rs` doesn't fit this file; the right name is
   `device_tests.rs`, and that's what I used. The brief said "if the
   file has no clean seam, say so in FINDINGS.md with what you tried —
   do not force one" — the records seam isn't there, so I picked the
   next-cleanest seam (tests) that does the work.

3. **`release.yml`'s `--no-fail-fast` is mostly cosmetic.** The gate
   already ran `cargo test` and already blocked the release job. Adding
   `--no-fail-fast` only changes behavior on failure (keep running to
   surface every failure rather than stopping at the first). Useful for
   triage, free in CI minutes, but the brief framed it as a correctness
   change and it's not — it's a developer-experience change.

Things the brief gets right and I didn't push back on:
- Plugin count drift was a real bug in two files (CONTRIBUTING and
  KDECONNECT_PROTOCOL). Both fixed.
- ROADMAP was listing shipped items as upcoming. Fixed.
- Tracked .deb / binary was a real anti-pattern. Already fixed by
  `b3db5ec`, no work needed.
- The release gate is a real safety improvement.

What the tests do not cover, and what could break in production that
the suite doesn't see:
- The test-module split is byte-identical, but the `cargo fmt` step
  rewrapped two lines (`Arc::new(ConnectionManager::new(...))` and a
  long `is_some_and` chain). If rustfmt behavior drifts between
  versions, future re-formats could push those lines again. Not a
  behavior risk, only a noise one.
- The pair/unpair family is still in `device.rs` and is still the
  largest function in the file (`pair_device` at 127 lines). The
  50-line rule from the constitution is violated there. The brief
  didn't ask me to split the function, so I didn't. A future change
  that touches `pair_device` should split it — the accept branch
  (lines 161-235) and the initiate branch (lines 236-281) are
  independently readable.
