# M3 Findings — shareinputdevices EI transport

## What changed

- **New module `src/plugins/shareinputdevices/ei.rs`** (≈870 lines): owns the `EiReceiver` reactor that takes the portal's `ConnectToEIS` fd, drives the reis handshake, binds the seat for Keyboard+Pointer+Button+Scroll, and pumps every EI event variant into the M1 planners (`plan_motion`, `plan_button`, `plan_scroll`, `plan_scroll_discrete`, `plan_key`).
- **Activation-id/sequence queue ported from `inputcapturesession.cpp:362-366` + `:394-404`**. The cpp tracks `m_currentEisSequence` (updated on every `DeviceStartEmulating`) and `m_currentActivationId` (updated when the D-Bus `Activated` signal arrives). Events arriving while `eis_sequence > activation_id` are queued; on `note_activated` the queue is drained in arrival order. This is load-bearing — the cpp added it to compensate for a race between EIS delivery and D-Bus `Activated` delivery — and the port preserves the exact ordering invariant.
- **`xkbcommon` integration for keysym/modifier lookup.** Mirrors `inputcapturesession.cpp:43-89`: `xkb_keymap_new_from_string` on the keymap fd from `DeviceAdded`, `xkb_state_new` from the result, `xkb_state_update_key` on every `KeyboardKey`, `xkb_state_key_get_one_sym` + `xkb_state_key_get_utf8` for text. Modifiers projected into the 4-bool `Modifiers { shift, ctrl, alt, super_key }` shape via `xkb_state_mod_name_is_active` with `STATE_MODS_DEPRESSED`. xkbcommon's Rust binding wraps `xkb_keymap_new_from_buffer` (explicit-length, NOT null-terminated), so the keymap fd is read with `SeekFrom::Start(0)` and trailing `\0` bytes are stripped defensively.
- **Wire-body stream + disconnect signal.** `start()` returns `(wire_rx, disconnect_rx, drive)`. `wire_rx` is the unbounded mpsc of `WireBody { Motion | Button | Scroll | Key(serde_json::Value) }`. `disconnect_rx` is a `watch::Receiver<bool>` flipped when the receiver sees `EiEvent::Disconnected` (the EIS fd EOF).
- **`tests/shareinputdevices_ei_socketpair.rs`**: 5 tests over a real `UnixStream::pair()` driving reis's `request::Connection` as a fake EI peer. Exercises the full handshake, seat binding, keymap fd delivery, and event sequence.

## How it was verified

- `cargo fmt --check` — clean.
- `cargo clippy --all-targets -- -D warnings` — clean (one targeted `#[allow(clippy::arc_with_non_send_sync)]` on `EiReceiver` and `xkb_state`, justified in a code comment — `xkb::State` is `!Send + !Sync` because it wraps a raw pointer to libxkbcommon's heap; access is single-threaded via a `LocalSet`).
- `cargo test --test shareinputdevices_ei_socketpair` — **5 / 5 pass**:
  1. `pointer_motion_round_trip` — emits `{"dx": 3.0, "dy": 4.0}` for a relative motion event after the activation gate is armed-then-cleared.
  2. `button_press_release_round_trip` — emits `{"singlehold": true}` on press, `{"singlehold": false}` on release (BTN_LEFT).
  3. `keyboard_keymap_loads_and_emits_text` — sends a memfd-backed xkb keymap with `<HKTG> = 43 → h`, fires `KeyboardKey(KEY_H=35)`, receives `WireBody::Key { key: "h", shift: false, ctrl: false, alt: false, super: false }`.
  4. `activation_gate_queues_until_activated` — `start_emulating(7)` followed by a motion is HELD until `handle_activated(7)` arrives, then the motion is emitted.
  5. `events_passthrough_when_not_armed` — without any `start_emulating`, events pass through immediately (default state).
- `cargo test --lib` — 1028 / 1028 pass (pre-existing shareinputdevices wire-shape and planner tests untouched).

## Critique — blunt

### What I left undone, and the reason

- **Task #4 (wire M3 into `PortalSession`) is not done.** `PortalSession::new` stashes the `ConnectToEIS` fd, but no code path passes it to `EiReceiver::new` yet. The tests drive the receiver directly via a `UnixStream::pair()`, which proves the receiver is correct in isolation but NOT that the portal→receiver hand-off works end-to-end. To finish: `PortalSession::setup_ei_receiver` (or similar) constructs an `EiReceiver` from the stashed fd and wires its `handle_activated` to the existing `Activated` signal handler. That belongs on the next lane (M4) because it depends on the `PortalSession` event-loop shape and the tests should be added there.

- **The `special_key` field on `WireBody::Key` is always 0.** The cpp computes a Qt::Key integer via `QXkbCommon::keysymToQtKey(sym, modifiers)`. There is no Rust equivalent in this crate yet, and reis doesn't expose keysyms directly either. M1's `plan_key` accepts an `i32` for `special_key`, so the transport emits 0 and the planner produces a body with `specialKey: 0` — which the phone treats as "use the text field". Adding a keysym→Qt::Key table is real work; it belongs in M4 (or a follow-up that adds an `xkb_keysym_get_name`/Qt::Key table on the planner side).

### Sharp edges hit during development (recorded so the next lane doesn't re-hit them)

1. **reis holds timestamped events in `pending_events` until a `Frame` arrives for the same device.** This is libei semantics, not a bug — every motion/button/key event needs a trailing `device.frame(0)` and `connection.flush()`. Test helpers do this; the production portal does it too, so this won't bite integration.

2. **`xkb_keymap_new_from_buffer` is NOT null-terminated.** The cpp calls the C function `xkb_keymap_new_from_string`, which IS null-terminated; the Rust binding calls the buffer variant, which is explicit-length. A trailing `\0` in the keymap text causes `[XKB-822] Failed to parse input xkb string`. Strip trailing NULs after reading from the fd (done defensively in `build_xkb_state`).

3. **The keymap fd delivered via SCM_RIGHTS does NOT have read position 0.** Even after `try_clone`, the cloned file's read offset is wherever the kernel left it. `file.seek(SeekFrom::Start(0))` is required before `read_to_end`. Without it, the read returns 0 bytes silently.

4. **xkb `level_name` requires a `[1]` subscript.** `level_name = "Any"` is a parse error in `xkb_types`; the correct form is `level_name[1] = "Any"`. The cpp masks this because Qt's xkb wrapper doesn't validate as strictly as xkbcommon does.

5. **`xkb::State` and `xkb::Context` are `!Send + !Sync`.** They wrap raw pointers. Wrapping in `tokio::sync::Mutex` does not make them `Send` (the inner `T` must be `Send` for tokio's Mutex to be `Send`). The receiver lives on a `LocalSet` and the Arc is shared only between tasks on that same set; clippy flags it, the allow comment explains why.

6. **`Arc<EiReceiver>` triggers `clippy::arc_with_non_send_sync`** at both the struct construction and the field-level `Arc::new(Mutex::new(None))` for `xkb_state`. Two separate `#[allow]` attributes needed; one at the struct level is not enough.

### Crate-spike decision: reis 0.7.1 (kept)

Spike from the brief — `reis` (pure-Rust libei) vs hand-rolled libei FFI. **Default `reis` if version/API fit is real; fall back to FFI.** Decision: **kept `reis`**. Evidence:

- reis's `EiEvent` enum covers every variant the cpp's `handleEiEvent` consumes (`inputcapturesession.cpp:344-449`): `Disconnected`, `SeatAdded`, `DeviceAdded`, `DevicePaused`, `DeviceResumed`, `DeviceStartEmulating`, `DeviceStopEmulating`, `PointerMotion`, `PointerAbsolute`, `Button`, `ScrollDelta`, `ScrollDiscrete`, `KeyboardKey`, `KeyboardModifiers`, `Touch*` (no-op'd because we don't advertise Touch).
- reis's `tokio` feature exposes `EiConvertEventStream`, an `async` `Stream<Item = Result<EiEvent, _>>` built on top of `handshake_tokio`. The handshake is non-blocking and the stream pumps events as fast as the EIS side produces them — the exact shape `EiReceiver::pump` needs.
- reis's `request::Connection` / `request::Seat::add_device` / `request::Device::interface::<T>()` high-level wrappers let the test fake a portal without writing a 120-line state machine (which is what the cpp does, and what hand-rolled FFI would also need to replicate).
- Hand-rolled FFI would replicate ≈30 C entry points + the full event state machine. Net maintenance cost is higher than the dependency.
- The only friction with reis: it re-exports `enumflags2::BitFlags` so we add it as a direct dep (already transitive via reis/tokio).

### Activation-id port — preserved

- `ActivationGate { eis_sequence, activation_id, pending: VecDeque<PendingInput> }` is the exact shape of the cpp's `m_currentEisSequence` / `m_currentActivationId` / `queuedEiEvents` triplet. The comparison `eis_sequence > activation_id` → queue; `eis_sequence <= activation_id` → pass-through. On `note_activated(activation_id)`, the queue is drained in arrival order (`std::mem::take(...).into_iter().collect()`) and returned to the caller (`PortalSession` in M4), which replays each event by re-entering the dispatch path with `should_queue() == false`.
- Test #4 is the load-bearing regression for this: motion fires while the gate is armed (seq=7, act=0, queue), the test then calls `handle_activated(7)`, and the motion emerges on the wire.

### Test counts

| | Before M3 | After M3 |
|---|---|---|
| `shareinputdevices_ei_socketpair` | 0 | **5** |
| `cargo test --lib` | 1028 | 1028 |
| `cargo clippy --all-targets -- -D warnings` | green | green |
| `cargo fmt --check` | green | green |

### Pre-existing failures NOT addressed by M3

- `tests/functional_coverage_lint::upstream_wire_provenance_is_consistent` fails because `provenance.yaml` references `src/plugins/shareinputdevices.rs` (single-file M1 layout) but the actual file is `src/plugins/shareinputdevices/mod.rs`. This is M1 refactor cleanup; out of scope for M3.
