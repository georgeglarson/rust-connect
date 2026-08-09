//! Network-change watcher (Task 2.2, vk #994)
//!
//! Single Responsibility: detect "the network topology may have changed"
//! and emit ONE debounced event per real change, so discovery can
//! re-announce (piece 2, `discovery_coordinator.rs`) instead of relying on
//! a periodic broadcast to eventually rediscover peers after roaming.
//!
//! ## Dependency pick: `if-watch`
//!
//! Evaluated `rtnetlink` (full netlink route-manipulation crate — this
//! daemon only needs to WATCH, not manipulate, so that's a bigger surface
//! than needed), hand-rolled raw netlink parsing (reinvents what a
//! maintained crate already does correctly), and `if-watch` (purpose-built
//! for exactly this, used by libp2p and others, tokio-native so no
//! competing async runtime enters the tree). `if-watch` won: it wraps
//! `rtnetlink` internally on Linux, so picking it directly instead of
//! hand-rolling costs nothing over what a raw-netlink implementation would
//! ALSO have needed (`netlink-packet-core`, `netlink-packet-route`,
//! `netlink-proto`, `netlink-sys`) — the marginal cost over raw netlink is
//! just `if-watch` itself, `fnv`, and `ipnet`. See
//! `plans/task-2.2-report.md` for the full dependency-tree diff against
//! the existing lockfile.
//!
//! ## Coverage, honestly stated
//!
//! `if_watch::tokio::IfWatcher` subscribes to `RTMGRP_IPV4_IFADDR |
//! RTMGRP_IPV6_IFADDR` (verified in the vendored source,
//! `if-watch-3.2.2/src/linux.rs`) — **address** add/remove only, not raw
//! link up/down and not default-route changes. In practice this covers
//! the roaming scenarios this task exists for (WiFi network switch,
//! dock/undock, VPN connect/disconnect, DHCP lease change) because a
//! NetworkManager/systemd-networkd-managed interface transition almost
//! always carries an address change with it. It does NOT cover a pure
//! link-flap with no address delta, or a default-route re-priority
//! between two already-up interfaces with no address change — named,
//! accepted gaps for this task's actual goal (laptop roaming, not
//! multi-homed routing edge cases). Escalation path if a live soak proves
//! this insufficient: watch `RTMGRP_LINK` / `RTMGRP_IPV4_ROUTE` directly
//! via the same `netlink-proto`/`netlink-sys` crates `if-watch` already
//! pulls in.
//!
//! ## Debounce
//!
//! kde's own debounce (`lanlinkprovider.cpp:73-75`,
//! `m_combineNetworkChangeTimer`) is a **0ms singleshot QTimer** — its
//! purpose is coalescing multiple SYNCHRONOUS signal emissions within the
//! same Qt event-loop tick, not waiting out a real-world flap over
//! milliseconds. That doesn't transfer here: netlink delivers a real
//! interface transition as several SEPARATE, asynchronously-arriving
//! messages (e.g. one `RTM_DELADDR` per address on link-down, then one or
//! more `RTM_NEWADDR` as DHCP re-acquires on link-up) spread over tens to
//! hundreds of milliseconds, which a same-tick-only debounce would not
//! coalesce at all. `DEBOUNCE_WINDOW` below is deliberately our own
//! evidence-based pick, not a copy of kde's value, documented as such
//! rather than citing a number that wouldn't mean what it looks like it
//! means.
//!
//! ## Resume-from-suspend detection
//!
//! Investigated two options per the brief:
//! - **netlink events on resume**: not reliable as the SOLE signal —
//!   whether resume produces a netlink event depends entirely on whether
//!   the interface actually re-negotiates an address (WiFi almost always
//!   does; a wired link with an unexpired DHCP lease or a static IP may
//!   not touch its address across a suspend cycle at all).
//! - **monotonic-clock jump watchdog**: `std::time::Instant` on Linux
//!   uses `CLOCK_MONOTONIC`, which per `clock_gettime(2)` **"does not
//!   count time that the system is suspended"** — verified against the
//!   man page this session. A watchdog built on `std::time::Instant`
//!   alone would never observe a suspend/resume jump; it isn't just
//!   unreliable, it's structurally incapable of the job.
//!
//! The fix: read `CLOCK_BOOTTIME` directly via `libc::clock_gettime`
//! (already a direct dependency, no new crate). `clock_gettime(2)`:
//! CLOCK_BOOTTIME is "identical to MONOTONIC, except that it also
//! includes any time that the system is suspended." Comparing BOOTTIME's
//! elapsed delta against MONOTONIC's over the same wall interval isolates
//! the suspended duration directly — during normal operation the two
//! clocks track each other almost exactly, so the gap is ~0; after a
//! resume the gap equals however long the suspend lasted. `CLOCK_REALTIME`
//! was considered and rejected: it's vulnerable to manual clock changes
//! and NTP corrections producing false positives, a problem MONOTONIC and
//! BOOTTIME are both immune to by design (same man page).

use std::time::Duration;

use futures::StreamExt;
use if_watch::tokio::IfWatcher;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// How long to wait after the LAST raw change event before firing one
/// debounced `NetworkChanged` notification (trailing-edge debounce — a
/// continuing flap keeps deferring, exactly like the reference
/// implementations' own single-shot-timer shape, just sized for our
/// actual event source; see the module doc for why kde's own 0ms value
/// doesn't transfer here).
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(750);

/// How often the suspend watchdog samples the two clocks.
const SUSPEND_WATCHDOG_POLL: Duration = Duration::from_secs(2);

/// Minimum apparent suspended duration to treat as a real suspend/resume
/// cycle rather than ordinary clock-read jitter between the two clocks
/// (which track each other almost exactly during normal operation) or a
/// brief lid-close-and-immediately-reopen not worth a full re-announce.
const SUSPEND_WATCHDOG_SLACK: Duration = Duration::from_secs(5);

/// Watches for network changes (address add/remove, and a suspend/resume
/// jump) and sends ONE debounced notification per real change on `tx`
/// until `shutdown` fires. Two raw sources feed the SAME debounce window
/// so a resume that also happens to touch an address doesn't double-fire.
pub async fn watch_network_changes(tx: UnboundedSender<()>, shutdown: CancellationToken) {
    let (raw_tx, raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let if_tx = raw_tx.clone();
    let if_shutdown = shutdown.clone();
    tokio::spawn(async move {
        run_if_watcher(if_tx, if_shutdown).await;
    });

    let suspend_shutdown = shutdown.clone();
    tokio::spawn(async move {
        suspend_watchdog(raw_tx, suspend_shutdown).await;
    });

    let raw_stream = UnboundedReceiverStream::new(raw_rx);
    debounce(raw_stream, DEBOUNCE_WINDOW, tx).await;
}

/// Address-change source: `if-watch` on Linux. Failure to start (e.g. no
/// permission to open a netlink socket, exotic sandbox) is logged and
/// degrades to suspend-watchdog-only coverage — never fatal, matching
/// every other backend-bearing subsystem in this codebase.
async fn run_if_watcher(tx: UnboundedSender<()>, shutdown: CancellationToken) {
    let mut watcher = match IfWatcher::new() {
        Ok(w) => w,
        Err(e) => {
            warn!(
                error = %e,
                event = "network_watcher_if_watch_unavailable",
                "Could not start the interface-address watcher; network-change \
                 re-announce degraded to suspend/resume detection only"
            );
            return;
        }
    };

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            event = watcher.next() => {
                match event {
                    Some(Ok(ev)) => {
                        debug!(event_kind = ?ev, event = "network_address_change", "Interface address change observed");
                        let _ = tx.send(());
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, event = "network_watcher_if_watch_error", "Interface watcher read error");
                    }
                    None => return,
                }
            }
        }
    }
}

/// Reads CLOCK_BOOTTIME via a raw `clock_gettime(2)` call — see the
/// module doc for why this, not `std::time::Instant`, is the clock this
/// watchdog needs.
fn read_boottime() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, correctly-sized out-parameter for
    // clock_gettime; CLOCK_BOOTTIME has been available since Linux
    // 2.6.39, long predating anything this daemon targets.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    if rc != 0 {
        // Essentially unreachable on Linux; degrade quietly rather than
        // panic — the watchdog simply won't detect a jump this cycle.
        return Duration::ZERO;
    }
    Duration::new(ts.tv_sec.max(0) as u64, ts.tv_nsec.max(0) as u32)
}

/// Suspend/resume source: compares CLOCK_BOOTTIME's elapsed delta against
/// `std::time::Instant`'s (CLOCK_MONOTONIC) over the same wall interval.
/// The gap between them is exactly the suspended duration — see the
/// module doc.
async fn suspend_watchdog(tx: UnboundedSender<()>, shutdown: CancellationToken) {
    let mut last_boottime = read_boottime();
    let mut last_monotonic = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = tokio::time::sleep(SUSPEND_WATCHDOG_POLL) => {}
            _ = shutdown.cancelled() => return,
        }

        let now_boottime = read_boottime();
        let now_monotonic = std::time::Instant::now();
        let boottime_elapsed = now_boottime.saturating_sub(last_boottime);
        let monotonic_elapsed = now_monotonic.duration_since(last_monotonic);
        last_boottime = now_boottime;
        last_monotonic = now_monotonic;

        let suspended_for = boottime_elapsed.saturating_sub(monotonic_elapsed);
        if suspended_for > SUSPEND_WATCHDOG_SLACK {
            info!(
                suspended_secs = suspended_for.as_secs(),
                event = "suspend_resume_detected",
                "Detected a CLOCK_BOOTTIME/CLOCK_MONOTONIC gap (suspend/resume); \
                 treating as a network change"
            );
            let _ = tx.send(());
        }
    }
}

/// Coalesces a burst of raw change events arriving on `raw` into ONE
/// emission on `tx`, fired `window` after the LAST raw event (trailing-
/// edge debounce). Decoupled from if-watch/netlink entirely so it's
/// directly unit-testable with a synthetic stream under a paused clock.
pub(crate) async fn debounce<S>(mut raw: S, window: Duration, tx: UnboundedSender<()>)
where
    S: futures::Stream<Item = ()> + Unpin,
{
    loop {
        // Wait for the first raw event of a new burst.
        if raw.next().await.is_none() {
            return;
        }
        // Keep pushing the deadline out while events keep arriving;
        // fire once `window` passes with no new event.
        loop {
            tokio::select! {
                next = raw.next() => {
                    if next.is_none() {
                        let _ = tx.send(());
                        return;
                    }
                    // another event arrived inside the window — loop
                    // again, which restarts the sleep below
                }
                _ = tokio::time::sleep(window) => {
                    let _ = tx.send(());
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    /// A single raw event, alone, fires exactly one debounced event after
    /// `window`.
    #[tokio::test(start_paused = true)]
    async fn test_debounce_single_event_fires_after_window() {
        let (raw_tx, raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let raw_stream = UnboundedReceiverStream::new(raw_rx);
        tokio::spawn(debounce(raw_stream, Duration::from_millis(100), out_tx));

        raw_tx.send(()).unwrap();

        // Nothing yet — window hasn't elapsed.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), out_rx.recv())
                .await
                .is_err(),
            "must not fire before the debounce window elapses"
        );

        // Advance past the window; the debounced event must arrive.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), out_rx.recv())
                .await
                .is_ok(),
            "must fire once the debounce window elapses"
        );
    }

    /// A burst of raw events within the window coalesces into exactly ONE
    /// debounced event — the flap-coalescing property this whole
    /// mechanism exists for.
    #[tokio::test(start_paused = true)]
    async fn test_debounce_coalesces_a_burst_into_one_event() {
        let (raw_tx, raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let raw_stream = UnboundedReceiverStream::new(raw_rx);
        tokio::spawn(debounce(raw_stream, Duration::from_millis(100), out_tx));

        // Simulate link-down + link-up as several messages arriving close
        // together, each one resetting the debounce deadline.
        for _ in 0..5 {
            raw_tx.send(()).unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        // Only after the LAST event's window elapses does it fire.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            out_rx.try_recv().is_ok(),
            "the burst must have coalesced into one debounced event"
        );
        assert!(
            out_rx.try_recv().is_err(),
            "a burst must produce exactly ONE debounced event, not one per raw event"
        );
    }

    /// Two bursts separated by MORE than the window produce two separate
    /// debounced events — debouncing must not merge genuinely distinct
    /// changes.
    #[tokio::test(start_paused = true)]
    async fn test_debounce_separate_bursts_produce_separate_events() {
        let (raw_tx, raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let raw_stream = UnboundedReceiverStream::new(raw_rx);
        tokio::spawn(debounce(raw_stream, Duration::from_millis(100), out_tx));

        raw_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(out_rx.try_recv().is_ok(), "first burst must fire");

        raw_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            out_rx.try_recv().is_ok(),
            "second, separate burst must also fire"
        );
    }

    /// A closed raw source ends the debouncer cleanly (used at shutdown).
    #[tokio::test(start_paused = true)]
    async fn test_debounce_ends_when_raw_source_closes() {
        let (raw_tx, raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let raw_stream = UnboundedReceiverStream::new(raw_rx);
        let handle = tokio::spawn(debounce(raw_stream, Duration::from_millis(100), out_tx));

        drop(raw_tx);
        tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("debounce must return promptly when the raw source closes")
            .expect("debounce task must not panic");
    }

    /// The production default is grounded in a real debounce need (not
    /// upstream's 0ms same-tick value, which wouldn't coalesce our async
    /// multi-message netlink event source at all — see the module doc).
    #[test]
    fn test_debounce_window_is_nonzero() {
        assert!(DEBOUNCE_WINDOW > Duration::ZERO);
    }
}
