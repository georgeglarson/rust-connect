# Notification re-dump on reconnect — dedupe shadow

Branch: `fix-notification-redump-shadow`. Base: main `3273bc8`.

## What changed

- Added a disconnect-surviving dedupe shadow on `NotificationPlugin`:
  `dedupe_shadow: Arc<RwLock<HashMap<(String, String), ShadowEntry>>>`
  (`src/plugins/notification.rs:125`).
- Added `ShadowEntry { signature, replaces_id, expires }` (`:148`) — a
  content hash and the last server id only, with a monotonic expiry.
  No servable content.
- Added `DEDUPE_SHADOW_TTL = 24h` (`:32`). Finite so a wrong entry
  self-heals within a day; long enough to cover overnight suspend
  without a morning re-dump.
- `dedupe_track` (`:477`) consults the shadow on a live-map miss only.
  Expired entries are pruned when encountered. A shadow hit restores a
  live entry so subsequent re-sends hit the live path:
  - same signature → `Suppress`
  - different signature, saved server id → `Replace(replaces_id)`
  - different signature, never shown (`replaces_id == 0`) → `Post`
- `on_disconnected` (`:691`) merges the device's live entries into the
  shadow before clearing, with merge-not-replace semantics (HashMap
  insert overwrites only the keys being inserted; other shadow entries
  stay). Also prunes expired entries for all devices.
- `dedupe_take` (`:576`) removes from both the live dedupe map and the
  shadow, so a phone-initiated cancel or a desktop-initiated dismiss
  forgets the notification completely across disconnects.
- Added a `#[cfg(test)] pub fn dedupe_shadow_size()` accessor (`:459`),
  mirror of `dedupe_size`.
- 9 new unit tests: 3 gate tests (`R1`–`R3`) + 6 companion tests.

## How it was verified

The claim is the 2026-09-03 live observation: ~130 desktop popups per
sleep/wake cycle, ~720 pile by evening. The unit tests simulate the
reconnect resync with the production dedupe code path and assert the
post-disconnect behavior changes from `Post` to `Suppress` /
`Replace(server_id)`.

Pre-fix run on main `3273bc8`, against R1/R2/R3 only:

```
cargo test --lib --locked --no-fail-fast -- \
  test_reconnect_resend_suppressed_by_shadow \
  test_reconnect_content_change_replaces_with_saved_server_id \
  test_shadow_is_per_device

# Result: 3 FAILED
# - test_reconnect_resend_suppressed_by_shadow:
#     assertion `left == right` failed
#     left: Post, right: Suppress
# - test_reconnect_content_change_replaces_with_saved_server_id:
#     left: Post, right: Replace(42)
# - test_shadow_is_per_device:
#     left: Post, right: Suppress
```

Each failure is the claim's own scenario: the just-emptied dedupe map
makes a reconnect resend `Post` a fresh popup. R1 asserts the new
shadow suppresses; R2 asserts the saved server id is reused for
changed-content replacement; R3 asserts the shadow is per-device.

Post-fix on this branch:

```
cargo test --lib --locked --no-fail-fast -- \
  test_reconnect_resend_suppressed_by_shadow \
  test_reconnect_content_change_replaces_with_saved_server_id \
  test_shadow_is_per_device
# test result: ok. 3 passed; 0 failed

cargo test --all-features --locked --no-fail-fast
# All test binaries green: 1143 lib tests + integration + doc tests
#   test result: ok. 39 binaries, 0 failed

cargo clippy --all-targets --all-features --locked -- -D warnings
# clean

cargo fmt --check
# clean
```

The bound test `test_shadow_map_stays_bounded_under_unique_id_flood`
(`src/plugins/notification.rs:2421`) floods `MAX_NOTIFICATION_HISTORY * 5`
unique ids, disconnects, and asserts `dedupe_shadow_size() <=
MAX_NOTIFICATION_HISTORY` — pinning the bound the brief promised.

## Critique — blunt

The brief is sound but not airtight. Where it could be wrong:

1. **The 24h TTL is the self-heal bound AND the bug window.** A wrong
   shadow entry (hash collision, bug, etc.) manifests as wrong desktop
   behavior (Suppress when it should Replace, or vice versa) for up to
   24h. The brief argues this is the right trade-off (vs a 1h TTL that
   re-dumps the morning pile, or an infinite TTL that buries bugs
   forever). 24h is a reasonable middle, but the choice is arbitrary.
   A daemon restart resets it anyway (daemon restart empties the
   shadow — acknowledged in the brief), so the worst case in practice
   is "wrong until next daemon restart OR 24h, whichever comes first."

2. **Hash collision is real but unrecoverable.** `content_signature`
   uses `DefaultHasher` (SipHash) over `(app_name, title, text)`.
   Collisions at 2^-64 are infeasible per-notification but the plugin
   serves the device for years. Two unrelated notifications could
   collide on signature → wrong Replace/Suppress. The brief doesn't
   mention this. The fallback (freedesktop treats unknown replace_id
   as a new post) absorbs the visible damage on the Replace side, but
   a wrong Suppress means a notification the user should see never
   pops. There is no in-plugin mitigation without a heavier signature
   scheme, and going heavier is out of scope.

3. **The race between `dedupe_track` (returns `Post`, inserts with
   `replaces_id = 0`) and `on_disconnected` (clears live, merges
   shadow with whatever replaces_id was current) can lose a
   freshly-assigned server id.** If `show_async` returns between
   `Post` and disconnect — narrow window, sub-100ms — then
   `dedupe_record_shown` is a no-op against the just-cleared live
   map, the shadow gets `replaces_id = 0`, and on the next resync
   the shadow returns `Post`. The original popup may still be on the
   desktop, so we duplicate. The brief says "do not try to validate
   liveness" — meaning we accept this. It's a narrow race and the
   duplicate is mostly harmless (user dismisses one), but the tests
   don't cover it.

4. **The shadow never GROWS past what the live map had.** If the
   phone's active-notification list on the second cycle contains an
   id that wasn't in the first cycle, the shadow doesn't have it,
   so the second-cycle arrival is correctly `Post`. Good. But the
   inverse — the phone dropping a notification the local history
   already evicted — is unobservable from the plugin (the phone
   simply stops sending it), so the shadow never has to forget it
   either. Fine.

5. **The shadow is per-plugin-instance. Daemon restart empties it.**
   Acknowledged in the brief. For the laptop, that's a daily minor
   annoyance on first reconnect after deploy. The brief calls this
   "deliberately out of scope — same shape as every other plugin
   state." OK.

6. **Tests don't exercise concurrent paths.** The implementation is
   RwLock-based with a strict `dedupe → shadow` lock order, but no
   test verifies deadlock-freedom under load. This matches house
   style (existing dedupe tests are single-threaded), but a stress
   test would catch a future regression that reverses the lock order.

7. **Lock acquisition in `dedupe_track` is now 2 write locks when the
   live map misses.** For the common case (live hit), it's 1 (same
   as before). The increase is on the reconnect resync path, which
   is the only path that matters for this fix. Acceptable.

8. **A persistently headless box never converges.** The shadow's
   `replaces_id == 0` Post path (headless/failed show) means a
   notification that never showed before will Post again on
   reconnect. On a box with no `DISPLAY` / `WAYLAND_DISPLAY`, the
   desktop path is skipped (see the show-side guard at the existing
   `if self.show_desktop && (DISPLAY || WAYLAND_DISPLAY)` site), so
   the live `replaces_id` never advances past 0. Every reconnect
   resync Posts every notification on the shadow. The history/SSE
   rows accumulate, the shadow entries persist. This is the same
   pre-existing property the live dedupe had — but the shadow makes
   it slightly more visible by extending the suppression window
   past the live map. Worth flagging; in practice, a headless box
   also has no desktop to flood, so the visible cost is zero.

Things I tried to break and couldn't:

- **Reorder the lock acquisition to `shadow → dedupe`.** Would
  deadlock against `dedupe_take` (which takes `dedupe` then
  `shadow`). The brief's lock-order rule forbids it; the impl
  follows the rule. A future code change that inverts it would
  deadlock under any concurrent dedupe access.
- **Have the shadow merge REPLACE all shadow entries for the
  device.** Would lose ids from earlier cycles that didn't make the
  current reconnect. `test_shadow_partial_resend_keeps_unarrived_ids`
  pins this.
- **Have `dedupe_take` not remove from shadow.** Would resurrect
  cancelled ids on every reconnect. `test_shadow_cancelled_ids_are_forgotten`
  and `test_shadow_dismissed_ids_are_forgotten` pin this.
- **Have the shadow not expire.** Would let a wrong entry persist
  indefinitely (until daemon restart). `test_shadow_entry_expires_after_ttl`
  pins this, including the "expired entry removed when encountered"
  property.
- **Have `on_disconnected` clear the shadow entirely for the
  device.** Would reintroduce the original bug. Every reconnect
  resend test (R1, R2, R3) would fail if this happened.
- **Have the shadow merge drop entries for other devices.** The
  brief says merge is for the disconnecting device only. The current
  impl iterates with `filter(|((dev, _), _)| dev.as_str() == device_id)`
  before collecting; cross-device shadow entries stay.
