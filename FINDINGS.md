# FINDINGS — mDNS storm sensor (`feat-mdns-storm-sensor`)

Internal review artifact. Lane brief: 2026-09-06 audit item 1. Class A.
Anchor commit: `427e1d0`. Branch: `feat-mdns-storm-sensor`.

## What changed

`src/protocol/mdns_discovery.rs` (+343 lines, 0 deletions):

- `pub(crate) const MDNS_STORM_THRESHOLD_PER_MIN: i64 = 600` — 10 sends/s;
  ~12× the steady-state rate measured on laptop 2026-09-06 (~0.8/s) but
  well below the ~100/s observed in the 2026-09-04 storm.
- `pub(crate) const MDNS_METRICS_SAMPLE_PERIOD: Duration = 60s` — the
  sample window. With `MissedTickBehavior::Skip` the timer never queues
  up work if the loop is busy.
- `const SEND_COUNTERS: &[&str]` — the outbound-packet-sent aggregate:
  `register-resend`, `unregister-resend`, `respond`, and the three
  `cache-refresh-*` counters. Citations in the source comment point at
  the exact lines in vendored mdns-sd 0.20.3 (`service_daemon.rs`).
  Explicitly excluded: `register`/`unregister` (counts commands received
  by the daemon thread, not packets sent), `known-answer-suppression`
  (counts suppressed answers, not packets), and the `timer`/`cached-*`/
  `dns-registry-*` family (state counts, not packet counts).
- `pub(crate) struct StormReport { sends: i64, deltas: Vec<(String, i64)> }`
  — deltas sorted by name, only counters that moved strictly up. A
  counter that went DOWN contributes 0 to sends and is absent from
  deltas; never a negative, never a panic.
- `pub(crate) fn storm_verdict(prev, cur, threshold) -> Option<StormReport>`
  — pure decision. Walks the union of keys in sorted order via
  `BTreeSet<&str>`, accumulates positive deltas, sums the in-aggregate
  ones, returns `Some` iff `sends >= threshold`.
- `async fn sample_metrics(daemon, last_metrics)` — one tick of the
  sampler. Asks the daemon for a metrics snapshot via `get_metrics()`
  (a `Receiver<Metrics>` on a bounded(1) flume channel), receives it
  inside a `tokio::time::timeout(2s)` so a stuck daemon can't park the
  runtime, and either stores the first snapshot silently or computes
  `storm_verdict` and logs `mdns_metrics` at DEBUG / `mdns_send_storm`
  at WARN. Every failure path (queue full, daemon gone, recv timeout)
  logs once at DEBUG and skips the sample — `last_metrics` is left
  untouched so the next tick has a valid baseline.
- The browse loop's `tokio::select!` gains a third arm
  (`_ = metrics_tick.tick() => sample_metrics(...).await`). The shutdown
  arm and the existing `receiver.recv_async()` arm are unchanged.

`CHANGELOG.md` (+7): `[Unreleased] / ### Added` bullet describing the
sensor and its `MDNS_STORM_THRESHOLD_PER_MIN`.

Red tests in `src/protocol/mdns_discovery.rs::tests`:

- `test_storm_verdict_register_resend_over_threshold` — the brief's
  register-resend+700 scenario; assert `Some(StormReport{sends:700})` at
  threshold 600, `None` at 800, deltas contains the moved counter.
- `test_storm_verdict_counter_reset_contributes_zero` — every counter
  went DOWN; verdict is `None` for thresholds 1/100/600/100_000.
- `test_storm_verdict_deltas_never_negative` — register-resend up by
  700 AND respond dropped by 450 (counter reset); verdict is `Some`,
  the `deltas` field has no negative entry, and `respond` is absent.
- `test_storm_verdict_cache_churn_is_not_a_storm` — only
  `cached-ptr` moved by 10 000; verdict is `None` at every threshold
  up to 10 000 000.
- `test_run_sampler_arm_does_not_kill_loop` — drives `run` with
  `start_paused = true`, advances 61 s so the 60 s metrics interval
  fires at least once, asserts the loop is still alive after the tick
  and shuts down cleanly on cancel. Uses a unique UUID-suffixed device
  id so the daemon doesn't contend with the other `our_identity()`-using
  tests in parallel runs (see Critique §2).

## How it was verified

### Red, before the implementation

Tests added first, no implementation in place:

```
$ cargo test --all-features --locked --lib \
    mdns_discovery::tests::test_storm_verdict_register_resend_over_threshold
error[E0425]: cannot find function `storm_verdict` in this scope
   --> src/protocol/mdns_discovery.rs:648:22
error[E0422]: cannot find struct, variant or union type `StormReport`
   --> src/protocol/mdns_discovery.rs:698:13
... 4 more `cannot find function `storm_verdict`` errors ...
error: could not compile `rust-connect` (lib test) due to 6 previous errors
```

Red confirmed — the symbols the brief specified didn't exist.

### Green, after the implementation

```
$ cargo build --locked
   Compiling rust-connect v0.1.0 (/tmp/delegate-rust-connect-feat-mdns-storm-sensor)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 25.54s

$ cargo test --all-features --locked --lib mdns_discovery
running 11 tests
test protocol::mdns_discovery::tests::test_resolved_to_identity_reads_reference_shape ... ok
test protocol::mdns_discovery::tests::test_resolved_to_identity_rejects_unusable_services ... ok
test protocol::mdns_discovery::tests::test_resolved_to_identity_falls_back_to_instance_name_for_id ... ok
test protocol::mdns_discovery::tests::test_storm_verdict_register_resend_over_threshold ... ok
test protocol::mdns_discovery::tests::test_storm_verdict_deltas_never_negative ... ok
test protocol::mdns_discovery::tests::test_storm_verdict_cache_churn_is_not_a_storm ... ok
test protocol::mdns_discovery::tests::test_storm_verdict_counter_reset_contributes_zero ... ok
test protocol::mdns_discovery::tests::test_run_sampler_arm_does_not_kill_loop ... ok
test protocol::mdns_discovery::tests::test_announce_then_browse_resolves_ourselves ... ok
test protocol::mdns_discovery::tests::test_reannounce_publishes_a_real_update ... ok
test protocol::mdns_discovery::tests::test_announcer_in_test_builds_is_invisible_to_production_browsers ... ok

test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 1159 filtered out; finished in 4.00s
```

Stable across 5 consecutive parallel runs.

```
$ cargo test --all-features --locked --lib
... 1170 passed; 0 failed; 0 ignored; 0 measured ...
```

Full lib suite (1170 tests) passes. (`test_copy_timeout_resets_when_progress_is_made`
in `payload_transfer` is a known timing-sensitive flake in the existing
suite — it passes alone and on re-runs; not related to this change.)

```
$ cargo clippy --all-targets --all-features --locked -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 13.78s

$ cargo fmt --check
(no output)
```

### Field scenario check (mental walk-through, not executed)

Steady state on laptop, ~0.8 sends/s, ≈48/min:

- 60 s sample 1: `last_metrics = None`, store baseline, silent. One
  `mdns_metrics` log line emitted? No — the brief says the first sample
  is silent, AND the `else` branch (DEBUG `mdns_metrics`) only runs when
  there IS a previous snapshot. So sample 1 produces zero log lines.
- 60 s sample 2: deltas are ~48, verdict `None` at threshold 600, one
  `mdns_metrics` DEBUG line with `sends_per_minute=48`. `journalctl
  --user -u rust-connect EVENT=mdns_send_storm` stays empty — matches
  the brief's oracle.

Storm scenario (~100 announces/s = ~6000/min, almost entirely
`register-resend`):

- 60 s sample 1: baseline.
- 60 s sample 2: `register-resend` delta ≈ 6000, `sends = 6000 ≥ 600`,
  one `WARN mdns_send_storm event="mdns_send_storm"
  sends_per_minute=6000 threshold=600 deltas="register-resend=6000"`
  line per minute while the storm persists.

## Critique — blunt

1. **Threshold choice is conservative.** `MDNS_STORM_THRESHOLD_PER_MIN =
   600` (10/s) is ~12× measured steady-state, well below the ~100/s
   storm peak. The right answer for a forensics sensor is "alerts at
   the first window where rate exceeded normal, not at 12×." A real
   `p99-over-baseline` heuristic would auto-tune this from a rolling
   baseline (e.g. warn when current-window > 3 × 1-hour rolling mean),
   at the cost of needing to persist a baseline across restarts and
   doing the math on every sample. The fixed-threshold approach is the
   right first cut for a forensics sensor ("what counter is climbing,
   at what rate"), not for a pager — but the next storm should
   revisit whether 600/min is the right alarm for the daemon's own
   ops team.

2. **The sampler arm adds parallel-test contention.** `run` now
   instantiates a `tokio::time::interval` and an `Option<Metrics>` for
   every spawned browse loop. With the existing
   `test_announce_then_browse_resolves_ourselves` and
   `test_reannounce_publishes_a_real_update` tests already exercising
   `run()` in parallel and BOTH hard-coding the same `our_identity()`
   device id, adding a third daemon-creating test pushed the parallel
   run from "stable" to "deterministically broken." The fix was to
   give `test_run_sampler_arm_does_not_kill_loop` a unique UUID-suffixed
   device id so it doesn't trample the existing tests' listen. This is
   a real cost: the existing two tests are ALSO broadcasting under the
   same id and would fail in parallel if a fourth such test were added
   tomorrow. The right fix is a `our_identity_uniq_for_test!` macro that
   hands every test its own id; left for another lane. **The brief's
   sampler test does not require the real-device resolution event** —
   I kept `our_identity()`'s shape for the construction path but
   changed the id, which is a deliberate narrowing of what the test
   proves (loop survives a sampler tick, vs. loop survives AND we
   observe our announcement). The brief's prior tests at `:552-600`
   cover the latter; the sampler test deliberately covers only the
   former.

3. **The 2 s `recv_async` timeout is a magic number.** If the daemon
   thread is genuinely overloaded the sample is skipped — which is the
   right behavior — but a chronically-overloaded daemon will skip
   every sample and the storm sensor will be silent right when it's
   most needed. The bound should be tunable via `RUST_LOG` or a config
   knob once a real storm hits and we learn what "normal" daemon-thread
   latency actually looks like under load. Today's value is a guess.

4. **`storm_verdict` builds a `BTreeSet` per call.** The aggregate is
   computed once a minute over ~20 counters — the cost is invisible.
   But if a future caller wants to call it in a tighter loop (e.g.
   per-second sampling, or a regression test that runs thousands of
   windows), the BTreeSet allocation dominates. A simpler
   `prev.keys().chain(cur.keys()).collect::<BTreeSet<_>>()` would
   short-circuit, but for a 1-minute cadence it's premature
   optimization.

5. **The "sends" aggregate deliberately excludes `register` and
   `unregister`.** Counting those would conflate "we asked the daemon
   to register once" with "the daemon sent N packets for it" — a
   single `register` call kicks off probing + announcing + several
   cache refreshes. The excluded counters are incremented in the
   command handler (`Command::Register` arm, line 3586), not on a
   packet-send path, so they don't measure the wire. The brief asks
   for the answer to this question to live in FINDINGS.md — it does,
   above and in the source comment. **Counter-argument:** a storm
   where `register` itself is being incremented at >10/s would mean
   something is calling `daemon.register` repeatedly, which IS a
   problem worth alerting on — but a different one (an upper-layer
   bug, not a wire storm). A second sensor covering "command-rate
   anomaly" would be a clean separation of concerns, not a fix here.

6. **What the sensor does NOT detect.** (a) A storm that bursts for
   <60 s won't cross the threshold; the next sample resets to baseline
   with no record. (b) `cached-ptr` going up by 600/min alongside
   legitimate browse traffic is invisible — the brief explicitly
   excludes cache counters, which is correct. (c) The sensor logs a
   single aggregate counter per minute; if three different announce
   types are all slightly elevated, the deltas field shows it but the
   `sends_per_minute` field is the sum, which is what tripped the
   threshold. A reader of the WARN line has to parse `deltas` to
   attribute the storm to a specific counter — which is exactly the
   forensics payoff the brief was after.

7. **A counter reset currently loses visibility silently.** If
   `register-resend` resets mid-storm (daemon restart, cache eviction
   of the counter map), the `saturating_sub` clamps to 0 for the
   window covering the reset, and we log no storm for that minute even
   if the underlying rate is still pathological. The right fix would
   be to track a `generation` counter (which `exec_command_get_metrics`
   could include in the snapshot) and treat a generation change as
   "first sample silent" again — but mdns-sd doesn't expose such a
   thing today. Documented as a known limitation; revisit if the
   next storm coincides with a daemon restart.

8. **I tried and could not break**: the WARN-line format. I tried
   (a) a reset of an in-aggregate counter (verdict correctly `None`),
   (b) `cached-ptr` movement alone (verdict `None` at every threshold),
   (c) mixing `register-resend` up 700 with `respond` down 450
   (verdict `Some`, sends=700, `respond` absent from deltas, no
   negative entries), (d) sampler arm with `start_paused = true` and
   61 s advance (loop survives, clean shutdown). The first three
   cover the `storm_verdict` contract; the fourth covers the sampler
   arm contract. The `2 s timeout + skip on failure` branch is
   exercised only by `cargo test --features failure-injection` (which
   we don't have), so a future test should mock `daemon.get_metrics`
   to return `Err` and verify the loop survives.
