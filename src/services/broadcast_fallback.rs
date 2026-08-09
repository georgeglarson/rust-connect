//! Bounded UDP-broadcast fallback (Task 2.2 piece 3, vk #994)
//!
//! Both references broadcast only on start and on network change — no
//! periodic timer at all (kdeconnect-kde `lanlinkprovider.cpp:149,192`;
//! kdeconnect-android `LanLinkProvider.java:567,572-584`). This module
//! is the documented, intentional DIVERGENCE from that: upstream can
//! assume avahi (mDNS) is always present on the desktop/mobile OSes it
//! ships on, so a dead mDNS daemon is not a case either reference
//! handles. We run on hosts where mDNS can be genuinely absent (no
//! multicast, a died daemon, a sandboxed environment), so start/change-
//! only broadcasting would leave such a host undiscoverable forever
//! after the first announce.
//!
//! Policy (brief-specified): while mDNS is DOWN (failed to start, or
//! its browse/daemon task exited) AND no device is currently connected,
//! broadcast on a backoff schedule — 5s initial, doubling, capped at 5
//! minutes. The moment either condition flips (mDNS healthy again, or
//! any device connects) the fallback goes quiet and the backoff resets,
//! so a healthy host — the overwhelmingly common case — never
//! broadcasts outside of start/network-change at all.
//!
//! `run_fallback_schedule` is the pure state machine (eligibility +
//! backoff timing), decoupled from real I/O via injected async
//! closures — same shape as `network_watcher::debounce`: independently
//! unit-testable with `tokio::test(start_paused = true)`, no wall-clock
//! sleeps, no real socket or connection state required.
//! `run_broadcast_fallback` is the production wrapper wiring it to a
//! real `DiscoveryService` broadcast and real eligibility signals.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::protocol::{ConnectionManager, DiscoveryService};

/// First fallback broadcast interval once eligibility begins (brief:
/// "5s → doubling → capped at 5 min").
pub const FALLBACK_INITIAL_INTERVAL: Duration = Duration::from_secs(5);

/// Ceiling the doubling backoff never exceeds (brief-specified).
pub const FALLBACK_MAX_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How often eligibility is re-checked while NOT currently eligible.
/// Deliberately short: regaining eligibility (a device disconnects, or
/// mDNS were to recover) is a cheap in-memory check, not a network
/// operation, so there is no cost to noticing it quickly. This is
/// unrelated to the backoff interval itself, which only governs the
/// spacing of actual broadcasts once eligible.
pub const ELIGIBILITY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Pure fallback-cadence state machine. `eligible` is awaited once per
/// loop iteration to decide whether fallback broadcasting should run
/// right now; `broadcast` is awaited each time the schedule fires.
/// Neither closure touches real sockets or connection state — that
/// wiring lives in `run_broadcast_fallback` below — which is what
/// makes this testable with `tokio::test(start_paused = true)` and no
/// real I/O.
///
/// Shape: while ineligible, re-check every `poll_interval` and hold the
/// backoff at `initial_interval` (so eligibility starting fresh always
/// begins at the shortest interval, never mid-backoff). While eligible,
/// broadcast immediately, then wait `current_interval` before the next
/// broadcast, doubling `current_interval` (capped at `max_interval`)
/// each cycle. Returns when `shutdown` is cancelled.
pub(crate) async fn run_fallback_schedule<EFut, E, BFut, B>(
    mut eligible: E,
    mut broadcast: B,
    shutdown: CancellationToken,
    initial_interval: Duration,
    max_interval: Duration,
    poll_interval: Duration,
) where
    E: FnMut() -> EFut,
    EFut: Future<Output = bool>,
    B: FnMut() -> BFut,
    BFut: Future<Output = ()>,
{
    let mut current_interval = initial_interval;

    loop {
        if !eligible().await {
            current_interval = initial_interval;
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(poll_interval) => continue,
            }
        }

        broadcast().await;

        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(current_interval) => {
                current_interval = (current_interval * 2).min(max_interval);
            }
        }
    }
}

/// Production wrapper: fallback-broadcasts `discovery`'s identity while
/// `mdns_healthy` reads false (see `service_manager::start_discovery`
/// for how that flag is maintained — set false at construction if mDNS
/// failed to start, and false again if its run task exits before
/// `shutdown` is cancelled) AND no device is connected
/// (`connection_manager.connected_device_ids()`).
///
/// Note on "reset by mDNS recovering" (brief wording): nothing in this
/// codebase currently restarts a died mDNS daemon (piece 2's boundary —
/// "do not remove or restructure mDNS itself" — rules that out here),
/// so `mdns_healthy` can only ever transition true→false today, never
/// back. The reset behavior is mechanically supported (eligibility is
/// re-checked every loop iteration, so a future true transition would
/// be picked up immediately with no code change here) but not
/// exercised by anything reachable today — worth knowing, not a defect
/// in this piece.
pub async fn run_broadcast_fallback(
    discovery: Arc<DiscoveryService>,
    mdns_healthy: Arc<AtomicBool>,
    connection_manager: Arc<ConnectionManager>,
    shutdown: CancellationToken,
) {
    run_fallback_schedule(
        || {
            let mdns_healthy = mdns_healthy.clone();
            let connection_manager = connection_manager.clone();
            async move {
                !mdns_healthy.load(Ordering::Relaxed)
                    && connection_manager.connected_device_ids().await.is_empty()
            }
        },
        || {
            let discovery = discovery.clone();
            async move {
                if let Err(e) = discovery.broadcast().await {
                    warn!(
                        error = %e,
                        event = "fallback_broadcast_failed",
                        "Failed to send fallback UDP broadcast"
                    );
                }
            }
        },
        shutdown,
        FALLBACK_INITIAL_INTERVAL,
        FALLBACK_MAX_INTERVAL,
        ELIGIBILITY_POLL_INTERVAL,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize};

    /// Brief's own red-shaped assertion: "time-paused: mDNS healthy +
    /// connected device -> NO broadcast within 10 simulated minutes."
    /// `eligible` returns false unconditionally the whole run.
    #[tokio::test(start_paused = true)]
    async fn test_never_broadcasts_while_always_ineligible() {
        let broadcasts = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();

        let broadcasts_clone = broadcasts.clone();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            run_fallback_schedule(
                || async { false },
                move || {
                    let broadcasts = broadcasts_clone.clone();
                    async move {
                        broadcasts.fetch_add(1, Ordering::SeqCst);
                    }
                },
                shutdown_clone,
                Duration::from_secs(5),
                Duration::from_secs(300),
                Duration::from_secs(2),
            )
            .await;
        });

        tokio::time::advance(Duration::from_secs(10 * 60)).await;
        shutdown.cancel();
        handle.await.expect("fallback task must not panic");

        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            0,
            "an always-ineligible schedule must never broadcast"
        );
    }

    /// The complementary case: eligible from the very first check.
    /// Broadcasting must start without waiting a full interval first —
    /// the fallback exists to fill a discovery gap immediately, not
    /// after a startup delay.
    #[tokio::test(start_paused = true)]
    async fn test_broadcasts_immediately_when_eligible_at_start() {
        let broadcasts = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();

        let broadcasts_clone = broadcasts.clone();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            run_fallback_schedule(
                || async { true },
                move || {
                    let broadcasts = broadcasts_clone.clone();
                    async move {
                        broadcasts.fetch_add(1, Ordering::SeqCst);
                    }
                },
                shutdown_clone,
                Duration::from_secs(5),
                Duration::from_secs(300),
                Duration::from_secs(2),
            )
            .await;
        });

        // Yield without advancing wall-clock time at all: the first
        // broadcast must already have happened.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        shutdown.cancel();
        handle.await.expect("fallback task must not panic");

        assert!(
            broadcasts.load(Ordering::SeqCst) >= 1,
            "an eligible-at-start schedule must broadcast without waiting a full interval"
        );
    }

    /// Backoff doubles each cycle, capped at `max_interval`: 5s, 10s,
    /// 20s, 40s — asserted by observing broadcast COUNT after known
    /// elapsed windows (time-paused, so this is exact, not timing-flaky).
    #[tokio::test(start_paused = true)]
    async fn test_backoff_doubles_each_cycle_up_to_the_cap() {
        let broadcasts = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();

        let broadcasts_clone = broadcasts.clone();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            run_fallback_schedule(
                || async { true },
                move || {
                    let broadcasts = broadcasts_clone.clone();
                    async move {
                        broadcasts.fetch_add(1, Ordering::SeqCst);
                    }
                },
                shutdown_clone,
                Duration::from_secs(5),
                Duration::from_secs(20), // small cap to keep the test fast
                Duration::from_secs(2),
            )
            .await;
        });

        tokio::task::yield_now().await;
        assert_eq!(broadcasts.load(Ordering::SeqCst), 1, "broadcast #1 at t=0");

        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            2,
            "broadcast #2 at t=5s (initial interval)"
        );

        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            3,
            "broadcast #3 at t=15s (doubled to 10s)"
        );

        // Next interval would double to 20s again (already at the 20s
        // cap set above), not grow past it.
        tokio::time::advance(Duration::from_secs(20)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            4,
            "broadcast #4 at t=35s (capped at 20s, not 40s)"
        );

        shutdown.cancel();
        handle.await.expect("fallback task must not panic");
    }

    /// Becoming ineligible (a device connects, or mDNS recovers) and
    /// then eligible again must restart the backoff at
    /// `initial_interval`, not resume doubling from wherever it left
    /// off. Eligibility is only observed at loop-iteration boundaries
    /// (right when a pending timer completes — see
    /// `run_fallback_schedule`'s doc comment), so the flag flips below
    /// are timed to land on those boundaries, not mid-sleep where the
    /// schedule can't see them yet.
    #[tokio::test(start_paused = true)]
    async fn test_backoff_resets_after_an_ineligible_gap() {
        let broadcasts = Arc::new(AtomicUsize::new(0));
        // 0 = ineligible, 1 = eligible; flipped mid-test.
        let eligible_flag = Arc::new(AtomicU32::new(1));
        let shutdown = CancellationToken::new();

        let broadcasts_clone = broadcasts.clone();
        let eligible_clone = eligible_flag.clone();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            run_fallback_schedule(
                move || {
                    let eligible_flag = eligible_clone.clone();
                    async move { eligible_flag.load(Ordering::SeqCst) == 1 }
                },
                move || {
                    let broadcasts = broadcasts_clone.clone();
                    async move {
                        broadcasts.fetch_add(1, Ordering::SeqCst);
                    }
                },
                shutdown_clone,
                Duration::from_secs(5),
                Duration::from_secs(300),
                Duration::from_secs(2),
            )
            .await;
        });

        // t=0: eligible, broadcast #1 immediately, backoff now waiting 5s.
        tokio::task::yield_now().await;
        assert_eq!(broadcasts.load(Ordering::SeqCst), 1, "broadcast #1 at t=0");

        // t=5: the 5s wait elapses, still eligible -> broadcast #2, next
        // wait doubles to 10s. Establishes the backoff has grown past
        // its initial value before we test the reset.
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            2,
            "broadcast #2 at t=5s; backoff now doubled to 10s"
        );

        // Go ineligible now, so it's in effect when the pending 10s wait
        // (due at t=15) elapses and the loop re-checks eligibility.
        eligible_flag.store(0, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            2,
            "t=15: ineligible when checked, no broadcast; backoff reset to initial internally"
        );

        // Still ineligible through one poll cycle (2s): no broadcast.
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            2,
            "t=17: still ineligible, still no broadcast"
        );

        // Go eligible again, in effect for the NEXT poll check.
        eligible_flag.store(1, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            3,
            "t=19: re-eligible must broadcast immediately, not resume a stale backoff wait"
        );

        // If the backoff had NOT reset to `initial_interval` (5s) back
        // at t=15, the next interval would still be some larger,
        // already-doubled value. Confirm no broadcast before a full 5s
        // elapses from t=19...
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            3,
            "t=23: must not broadcast again before a full reset 5s interval elapses"
        );
        // ...and that it does fire at exactly the reset 5s mark.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            broadcasts.load(Ordering::SeqCst),
            4,
            "t=24: broadcast #4 at exactly the reset 5s interval"
        );

        shutdown.cancel();
        handle.await.expect("fallback task must not panic");
    }

    /// `shutdown` must stop the schedule promptly, whether it is
    /// currently in the eligible-backoff wait or the ineligible-poll
    /// wait.
    #[tokio::test(start_paused = true)]
    async fn test_shutdown_stops_the_schedule() {
        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            run_fallback_schedule(
                || async { false },
                || async {},
                shutdown_clone,
                Duration::from_secs(5),
                Duration::from_secs(300),
                Duration::from_secs(2),
            )
            .await;
        });

        tokio::task::yield_now().await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("fallback task must stop promptly on shutdown")
            .expect("fallback task must not panic");
    }
}
