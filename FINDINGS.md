# M3 panel round 3 — FINDINGS

Lane: `fix-shareinputdevices-m3-panel-r3-fin` (head 7b2a83b).
Six pre-freeze commits (ada4f90 .. f9b4bd5 = fixes 1-7) plus
seven hygiene commits (0af8482 .. 7b2a83b = fix 8 + fmt).
The commit messages carry the substance per item; this file
summarizes the lane, runs the gates, and critiques the brief.

## What changed

### Pre-freeze commits (fixes 1-7, six commits)

- **3e98697 — contentless-key suppression + all-or-nothing filter.** Empty
  text + specialKey 0 bodies are dropped at the source; the
  `keysym_to_text` filter switches from per-char `< 0x20` strip to
  all-or-nothing. Adds `control_char_keys_emit_no_body_until_m4_table`
  (Escape) and `cursor_keys_emit_no_body_until_m4_table` (Up/Down/Home/End).
- **f9b4bd5 — trailing-NUL strip is load-bearing.** The strip in
  `build_xkb_state` matches the production Wayland payload
  (`size = strlen + 1`); comment was rewritten to say so and
  `keymap_fd_with_trailing_nul` plus `keymap_with_trailing_nul_parses_and_emits_text`
  pin the production shape.
- **f94f303 — keymap-parse fallback to default RMLVO keymap.** Mirrors
  cpp `inputcapturesession.cpp:61-64`; on parse failure warn + try
  `xkb::Keymap::new_from_names` with empty strings. Red-before-green
  test `garbage_keymap_falls_back_to_default_and_key_delivery_survives`.
- **43e5c17 — `EiReceiver` is `Send + Sync`.** Moves `xkb::State` out of
  the struct into the pump future so the Arc held by PortalSession is
  shareable on the multithread runtime; adds the
  `assert_send::<Arc<EiReceiver>>()` compile-time pin.
- **cb28541 — replay-order window.** `handle_activated` now holds
  gate → wire across `note_activated`, closing the window where a
  pump event could pass through ahead of the replayed queue.
- **ada4f90 — packaging + docs.** `libxkbcommon0` added to deb
  `control`; `libxkbcommon-dev` documented in `CONTRIBUTING.md` and
  `packaging/build-deb.sh`.

### Fix 8 (seven commits)

- **0af8482 — drop duplicate `disconnect_tx.send` in pump Disconnected.**
  The arm + trailer both sent it; the trailer covers every path, the
  arm now just breaks and lets the trailer fire.
- **d6cb9ee — drop `Error::Io`, broaden `Error::Xkb` label.** Only one
  site (`Context::new`) reaches the io-error variant; folds into
  `Error::Reis` via `.map_err(ReisError::Io)`. `Xkb` label moves from
  "xkb keymap parse failed" to "xkb keymap load failed" (the variant
  already wraps try_clone / read / utf8 / parse / fallback).
- **3532393 — use xkbcommon crate `MOD_NAME_*` constants.** Drop the
  four local `MOD_*` consts; verified in xkbcommon 0.9
  (`src/xkb/mod.rs:267-274`) the constants exist with values
  byte-for-byte identical.
- **a28cfc8 — drop direct `enumflags2` dep, import via `reis`.** reis
  0.7.1 has `pub use enumflags2;` at `lib.rs:38`; both call sites
  import via `reis::enumflags2::BitFlags`. `Cargo.lock` loses the
  rust-connect → enumflags2 edge.
- **619a337 — handle_activated Button drain comment.** Rewrote the
  contradictory justification: the live path queues raw
  `PendingInput::Button` and the drain check is what catches Null at
  the gate-queuing path's rebuild site, not a duplicate of the
  pass-through check.
- **9628ba2 — test comment hygiene batch.** `keymap_fd` first paragraph
  dropped (the NOTE carries the substantive info); `seat_bind_reaches_…_before_devices`
  renamed to `seat_bind_reaches_the_eis_peer` (the test never calls
  `add_device`, so the "before devices" framing was unasserted);
  `kb.modifiers(...)` call sites switched from trailing inline comments
  to leading labels (the first arg, `serial`, was unlabeled, so the
  trailing labels "sat one position off"); EOF-test teardown comment
  rewritten to describe what the drop actually does (the pump is
  abandoned at teardown, lifetime bounded by the runtime not the wire).
- **7b2a83b — `cargo fmt`.** Reflowed `Keymap::new_from_names(...)` and
  the two `kb.modifiers(...)` call sites; no semantic change.

## How it was verified

All three gates run after the seven hygiene commits land, against
the warm `CARGO_TARGET_DIR=$HOME/.cache/rust-connect-target-m3-ei`,
full suite (no TMPDIR override per standing requirement).

### `cargo fmt --check`

Clean. No diff.

### `cargo clippy --all-targets -- -D warnings`

Clean.

```
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 21.72s
```

### `cargo test --no-fail-fast`

```
test result: ok. 1040 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.33s
test result: ok. 33 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.56s
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 41 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.76s
... [29 more binaries, all green]
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.45s

Total: 1223 passed; 0 failed; 4 ignored (1 doc, 3 usb_integration requiring a real Android device on adb).
```

Full output captured at `/tmp/test-results.txt` (31 test binaries).

### Per-fix red→green

The six pre-freeze commits have their red-before-green tests in the
diff (see each commit message); the host reboot killed the lane before
I could observe them. The hygiene batch (fix 8) is mostly comment /
constant / dep-reshaping work — by design, no red→green tests added
because each item is bounded refactoring, not new behavior. The one
behavior change in the batch — `Error::Io` removal — would require a
test that triggers `ei::Context::new`'s io error (a non-blocking
`set_nonblocking` failure on a closed fd); not worth the harness cost
for a one-line `.map_err(ReisError::Io)`.

## Critique — blunt

### Fix 1 (contentless suppression) is environment-fragile

The `control_char_keys_emit_no_body_until_m4_table` test asserts
Escape never produces a body. That assertion is true ONLY because
the production-shape xkb mapping of `<ESC>` resolves to `XK_Escape`
whose UTF-8 representation contains `\x1b`. If a future xkbcommon
release or a custom keymap mapped `<ESC>` to a printable keysym, the
suppression would silently stop firing (the text wouldn't be empty)
and the test would assert the OPPOSITE — contentless bodies would
reappear. The test pins a single bytes→keysym resolution outcome, not
a property. The "until M4's keysym→Qt::Key table" deferral annotation
on `plan_key` is the load-bearing comment; the test name
`emit_no_body_until_m4_table` makes the dependency explicit, but the
assertion is brittle in a way that the round-2 reviewer is supposed
to push back on. I tried to break it by skimming the keymap-to-text
path for any other code path that could re-introduce a non-empty
body and could not find one in the current code — but the dependency
on the exact bytes xkbcommon emits for `<ESC>` is real and unasserted.

### Fix 5 (default-keymap fallback) is environment-dependent and asserted weakly

`garbage_keymap_falls_back_to_default_and_key_delivery_survives`
asserts that after a garbage keymap, `KEY_H` produces a `WireBody::Key`
with non-empty text. That assertion is true ONLY when the host's
default RMLVO keymap binds H to a printable keysym. The test
explicitly does NOT pin the exact text for this reason. But the
deeper question — does the fallback ACTUALLY install a usable
keymap, or does the receiver just leak a parse-failed state and
the test happens to pass because the keymap-less path silently drops
all keys? — is not asked. A test that asserts a wire body's
`mods.shift == false` (the default) for a parallel shift press would
be the stronger oracle. The current assertion collapses two
outcomes into one.

### Fix 7 (trailing-NUL strip) is environment-coupled to libei but the test isn't

`keymap_with_trailing_nul_parses_and_emits_text` covers the strip's
correctness on the receiving end. It does NOT cover the OTHER side
of the convention: does libei / reis send `size = strlen + 1` in
practice, or `size = strlen`? The test exercises the receiver against
a keymap fd the TEST builds with `keymap_fd_with_trailing_nul`. If a
real EIS (mutter, KWin, gnome-remote-desktop) actually sends `size =
strlen` because its libei version was compiled against a different
convention, the receiver would still strip nothing, still parse, and
still emit — the test passes but production matches a different
shape. I tried to break this by reading reis 0.7.1's request path to
see whether it tracks the on-the-wire size or recomputes strlen
client-side, and the answer is: reis's `keymap` request does pass the
size through verbatim, so what the sender put in the wire IS what
the receiver sees. The convention is the sender's, not reis's. The
test pins what the helper does, not what production sends.

### Fix 8d (drop enumflags2 direct dep) leaks Cargo's resolver semantics

`pub use enumflags2;` in reis's lib.rs is a module-level re-export.
Importing as `reis::enumflags2::BitFlags` works because reis re-exports
the enumflags2 module, and Cargo's resolver brings in enumflags2
transitively (via reis). If a future reis release drops that
re-export (or moves enumflags2 behind a feature flag), our import
breaks at compile time with no warning. The fix doesn't pin this —
there's no `#[deny(missing_docs)]` or similar to catch the breakage.
I tried to break this by reading reis's published API guarantees and
couldn't find a stability commitment for the re-export. If reis
moves enumflags2 behind `cfg(feature = "enumflags2")`, our crate
silently can't upgrade.

### Fix 8b (drop `Error::Io`) is a net good but loses an explicit signal

The brief labeled `Error::Io` "never-constructed" but the `?` in
`EiReceiver::new` was implicitly constructing it via
`#[from] std::io::Error` (reis's `Context::new` returns
`io::Result<Self>`). The brief was wrong about reachability. The
fix — fold into `Error::Reis(ReisError::Io)` — is the right call
(one site, one shape), but the call site now silently conflates a
set_nonblocking failure with every other reis-side error. The
`map_err(ReisError::Io)` makes the channel obvious, but a future
caller reading the `Error` enum won't know the `Reis` arm carries a
stdlib io::Error in that one specific case unless they read the
call site. The Error doc block tries to make this clear ("the
io-error variant is reachable only here") but a maintainer
reviewing the enum alone could miss it.

### The lane-wide pattern: bounded hygiene batch is bounded in a way that bounds the review

Fix 8 is six bounded refactoring items — no behavior changes, no new
tests, no new public surface. That's the right shape for a final-pass
hygiene batch (you don't want scope creep the round before merge), but
it also means the "How it was verified" section above is dominated
by the gate results, not by behavior tests. The gates confirm the
code compiles and the existing suite still passes; they do NOT confirm
the refactorings preserved the behavior the prior fixes were supposed
to add. A future regression in, say, the `MOD_NAME_LOGO` swap (if
xkbcommon 0.10 renames it) would be caught at compile time because
the constant path is statically resolved — but a regression in the
handle_activated replay-order fix would need a focused test that the
existing suite may or may not cover. I scanned for a test that races
a pump event against `handle_activated` and didn't find one that
specifically pins "no event lands between the gate disarm and the
replay"; the round-2 commit message claims such a test exists, but
the test names don't include that scenario.
