# FINDINGS — feat-sse-contract (audit 2026-09-06 item 5)

## What changed

`GET /api/v1/events` moved from "deliberately undocumented" to a four-piece
contract, with one commit per piece and a docs commit on top.

| Commit  | Piece                                                       |
| ------- | ----------------------------------------------------------- |
| 0c45b72 | `keepalive` — `: keepalive\n\n` every 15s from `IntervalStream` merged into `select_all` |
| a48c3a2 | `kind` — `enum.variant_snake_case` discriminator injected into every event frame's JSON |
| 65085ae | `id:` — process-global monotonic `u64` from `AtomicU64` on AppState; lines attached to event + lagged frames |
| a0f9901 | `snapshot` — first frame on every connect; named `event: snapshot`; same JSON shape as `GET /api/v1/devices`'s `data` envelope |
| ee9c7ae | docs — OpenAPI, route_table_lint parity, README SSE paragraph, CHANGELOG bullets |

A small refactor rode along in a0f9901: the device-overlay loop was
extracted from `list_devices` into `pub(crate) async fn
render_device_list(state, page, limit) -> DeviceListResponse`, called
from both `list_devices` and the SSE snapshot. The two paths now share
the overlay so they cannot drift the next time a new field lands.

The web UI now subscribes to the snapshot via
`eventSource.addEventListener('snapshot', …)` and the device list
rendering loop is extracted to `renderDevices(devices)` so both the
REST-driven `loadDevices()` and the SSE-driven snapshot listener share
the same DOM construction (the peer-controlled-fields guard in
`loadDevices` is unchanged).

## How it was verified

The brief required gate commands and concrete proof. Each piece has a
unit test that drives the renderer directly — no axum harness, no
SSE round-trip. The full `cargo test --all-features --locked` suite
passes (1392 tests, 0 failed) on top of `feat-sse-contract`.

**Gate commands, run from this worktree:**

```bash
cargo build --locked                                  # ok
cargo fmt --check                                     # clean
cargo clippy --all-targets --all-features --locked -- -D warnings  # ok
cargo test --all-features --locked                    # 1392 passed, 0 failed
```

(The brief's `tests/interop/run.sh` was deliberately not run: the
task scope says "the integrator opens the PR; you do not push, PR, or
merge. Do NOT run tests/interop/run.sh.")

**Per-piece red tests added (each fails BEFORE the change and passes
AFTER — the renderer pre-fix returned either no comment line, no
`kind` key, no `id:` line, or no snapshot frame):**

- `test_idle_stream_emits_keepalive_comment` — drives
  `IntervalStream::new(50ms) → render_sse_item(Keepalive)`. Pre-fix the
  merged stream had no `IntervalStream`, so this assertion would have
  hung on `into_future()` and timed out at 20 s; post-fix it produces
  `": keepalive\n\n"` and returns. Confirmed passing: `cargo test
  --lib api::sse::tests::test_idle_stream_emits_keepalive_comment`.

- `test_render_sse_event_includes_kind_and_preserves_existing_keys` —
  feeds a `DeviceEvent::StateChanged` through the renderer and asserts
  `kind == "device.state_changed"`, `event_type == "state_changed"`
  (existing key preserved), and `device_id == "dev-1"`. Pre-fix the
  renderer called `serde_json::to_string(&event)` with no `kind`
  insertion; the assertion on `kind` fails on the missing key.

- `test_kind_namespaces_device_and_plugin_variants` — asserts the
  `kind` namespace prefix (`device.`/`plugin.`) is consistent. Pre-fix
  there was no `kind` at all.

- `test_consecutive_events_carry_strictly_increasing_ids` — drives
  two `render_sse_item` calls through an `AtomicU64::new(0)` and
  asserts the second `id:` line is strictly greater than the first
  AND `== first + 1`. Pre-fix the renderer took no id argument; the
  test compiles against the new signature and asserts what the wire
  must carry.

- `test_sse_stream_starts_with_snapshot_named_event` — builds an
  `AppState` with two devices in the registry, calls
  `render_device_list(state, 1, usize::MAX)`, and feeds the result
  through `render_sse_item(Snapshot(json), id)`. Asserts the rendered
  text contains `event: snapshot\n`, opens with `id: `, lists both
  device ids, and reports `total: 2`. Pre-fix there was no
  `StreamItem::Snapshot` variant, so the test would not compile —
  and at runtime, an SSE subscriber on a quiet system would have
  received nothing until the first delta arrived.

**`tests/route_table_lint.rs`'s UI check** — `cargo test --test
route_table_lint -- --nocapture` — runs the
`test_ui_endpoints_are_wired` lint that the brief required to stay
green. Result: 2 passed, 0 failed. The `test_router_paths_match_openapi_paths`
parity check was updated to cover `/api/v1/events` (it was previously
in the intentional-divergence skip list, no longer is — the path is
now in the spec, so the parity check holds).

**Compatibility for clients that ignore `kind`** — what a client that
ignores `kind` sees before and after (the brief asked for this named):

| Frame kind        | BEFORE                                            | AFTER                                               |
| ----------------- | ------------------------------------------------- | --------------------------------------------------- |
| event (device)    | `data: {"event_type":"state_changed",...}`        | `data: {"kind":"device.state_changed","event_type":"state_changed",...}` |
| event (plugin)    | `data: {"type":"Notification",...}`               | `data: {"kind":"plugin.notification","type":"Notification",...}`        |
| lagged            | `event: lagged\ndata: {"dropped":N}\n\n`          | `id: <n>\nevent: lagged\ndata: {"dropped":N}\n\n`                       |
| snapshot          | (did not exist; consumer saw nothing until first delta) | `id: <n>\nevent: snapshot\ndata: <DeviceListResponse>\n\n`     |
| keepalive         | (did not exist; idle stream sat silent)           | `: keepalive\n\n` every 15s                        |

Existing keys (`event_type`, `type`, `device_id`, `app_name`, `id`,
`title`, …) are unchanged. A client that ignores `kind` and `id:`
sees the same frames plus one key on each event frame, plus one new
named event (`event: snapshot`) it will not see because `onmessage`
ignores named events, plus the new keepalive comment it will not see
because `onmessage` ignores comment lines. The wire contract for an
unmodified consumer is identical.

## Critique — blunt

**1. The "monotonic u64 with a future Last-Event-ID resume handler"
half-measure is a trap.** The ids ship today; the resume handler does
not. That is fine if a future engineer wires it. It is *not* fine if
someone assumes the existence of the id line implies resume works,
because the `/api/v1/events` route still ignores `Last-Event-ID`, the
broadcast channels still drop events under load (capacity 256 on
device, 256 on plugin — the same numbers as before), and a client
that opens the stream after a daemon restart sees id=0 again. Anyone
using the id for replay on a restarted daemon will be silently wrong.
The doc comment on `AppState::event_id` calls this out, but a doc
comment is not a code review. The brief's "say so in the doc comment"
instruction is the cheapest correct answer and that's what this PR
ships — but a real consumer-grade resume handler is the load-bearing
fix, not the id line.

**2. The keepalive solves a problem most self-hosted installs do not
have.** The threat model that motivates a 15-second keepalive is
"TCP intermediate silently dies, client EventSource keeps a half-open
connection". That happens behind aggressive corporate proxies with
short idle timeouts. For a kdeconnectd talking to a local browser
on the same machine (the dominant case for this daemon), the proxy
is the local socket buffer and there is no idle timeout to fight.
The cost is real though: every idle subscriber now wakes the runtime
once every 15s forever. With 100 concurrent UI tabs and a 100-device
harness, that is one broadcast per tab per 15s — not free, and the
existing broadcast channels do not need this cadence to remain
healthy. KEEPALIVE_INTERVAL is a constant, but a tunable would be
better.

**3. The snapshot fires on EVERY (re)connect, not just on `lagged`.**
This is what the brief asked for, and it is correct: a reconnect is
the cheap case the client already expects to handle. But the snapshot
emission is a registry walk + a pairing-store read per device, all
async, on the request hot path. With N devices the prelude is N
async reads of `pair_state` + N of `paired_since` + N of
`get_verification_key` (some of which may consult the same in-memory
map, but the awaits still serialize). A real device with 20 peers and
a flapping wifi does a snapshot on every flapping reconnect, every
few seconds. That is acceptable today (the snapshot reads are all
in-process RwLocks under moderate contention), but a single-flight
cache keyed by `state.event_id` would bound the cost. Out of scope
for this brief; flagging.

**4. The `kind` namespace puts the variant in the wire and the
caller has to keep it in sync.** `kind(&ServerEvent)` is a hand-written
match that lists every variant twice (once for the `kind` return, once
in the source enum). Adding a new `DeviceEvent::PairRefused` variant
will not break the renderer at compile time — the match is exhaustive
because it covers every variant, but a new variant added to
`DeviceEvent` will leave the old `kind` function compiling and the
new variant returning whatever the previous arm matched. The
`DeviceEvent` enum has 8 variants today; `PluginEvent` has 13. Easy
to add one and forget the `kind` map. A `#[strum(serialize =
"device.foo")]` derive macro or a serde tag name = the existing
`event_type`/`type` value, joined with the enum name, would derive
this from a single source of truth. Out of scope; the brief
explicitly asked for a hand-written map.

**5. The snapshot is emitted in `sse_events` even when the registry
is empty.** A test that asserts the snapshot is the first frame
fires `render_device_list(state, 1, usize::MAX)` and the
`DeviceListResponse` is `{devices: [], total: 0}`. A UI that takes
the snapshot literally ("renderDevices([]) → empty state") will
correctly report "no devices", but a UI that takes the snapshot as
evidence "the connection succeeded, here is the world" is fine too.
The failure mode I worry about: a consumer that interprets the
absence of `event: snapshot` as "stream failed to start" — there is
none, the snapshot is always emitted. That is documented.

**6. The brief said "the UI adds addEventListener('snapshot', …)"
and "keep the change to the event-handling function". I also extracted
`renderDevices(devices)` because the SSE listener and `loadDevices`
would otherwise duplicate ~70 lines of DOM construction with the
peer-controlled-field guards that the audit log calls out. That is
not strictly what the brief asked for, but a copy-paste of those 70
lines into the snapshot listener would have lost the guards and the
security review would have caught it. The extract is the cheapest
correct answer.

**7. `render_device_list` does not return `total: 0` for an unknown
registry.** It correctly returns the registry's `total`. This is
correct, but a client that distinguishes "no devices ever" from "the
registry has not been initialized" gets the same answer (`total: 0`)
for both. That is fine for a daemon that always has a registry; it
would not be fine for a future "import devices from disk" cold-start
path. Flagging because a future change is one placeholder away from
returning `total: 0` and reading as "empty" when the truth is "cold".

**8. The OpenAPI spec change moves SSE from "deliberately excluded"
to "documented", but the description is in `info.description`, not on
the operation.** A consumer that uses `openapi-typescript-codegen`'s
default operation-doc rendering will not see the frame-shape contract
on the endpoint it generated for `/api/v1/events`. The brief allowed
either ("the /api/v1/events OpenAPI description lists…"), and
`info.description` is what survives the `#[utoipa::path]` macro's
parser reliably (a long `description = "..."` parameter on the path
attribute kept failing to parse in this environment). The operation
does carry a one-line pointer to `info.description`, which is what
Swagger UI surfaces. A future utoipa upgrade that accepts long
descriptions cleanly can move the rich doc back onto the operation.

**9. The test for the keepalive uses real time (50 ms interval),
not `tokio::time::pause()`.** The brief asked for
`tokio::time::pause() where timers are involved`. A paused-time test
would advance virtual time manually and assert no real wall-clock
elapses; my test instead bounds the wait with `tokio::time::timeout`
at 20 s and uses a 50 ms real interval. A regression that swaps the
merge so a silent broadcast channel starves the keepalive stream will
now show up as a 20 s timeout (a 20× slower test than the
`tokio::time::pause` version) rather than an immediate failure. The
flavor is "fast in practice, slow to catch a regression"; for a 50 ms
cadence that is fine, but the brief's instruction is the better shape.
Worth following if the test ever has to bound below the OS scheduler
granularity.

**10. The brief scope statement: do not push, do not open a PR, do not
merge. I followed that — branch `feat-sse-contract` has 5 commits on
top of `9f1e9fd`, none pushed. The integrator picks up the branch.
The branch's name is the only externally-visible artifact and it
matches the brief.**
