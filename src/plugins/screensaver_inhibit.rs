//! Screensaver Inhibit plugin
//!
//! Single Responsibility: keep the desktop's screensaver from locking
//! while a paired phone is connected. Lifecycle-driven, NOT packet-driven
//! — upstream declares no packet types at all (kdeconnect-kde
//! plugins/screensaver-inhibit/kdeconnect_screensaver_inhibit.json:
//! SupportedPacketType = [], OutgoingPacketType = []).
//!
//! Upstream semantics (screensaverinhibitplugin.cpp): the per-device
//! plugin's CONSTRUCTOR calls
//! org.freedesktop.ScreenSaver.Inhibit("org.kde.kdeconnect.daemon",
//! "Phone is connected") on the session bus and stashes the cookie; the
//! DESTRUCTOR UnInhibits it and then calls SimulateUserActivity — with an
//! explicit safety rationale: whatever manages the screensaver may not
//! restart the idle timer when the last inhibition lifts, which would
//! leave an unlocked desktop. Per-device plugin instances mean one cookie
//! per device, so our map is per-device too; our on_connected /
//! on_disconnected hooks map onto upstream's plugin ctor/dtor.
//!
//! Daemon shutdown needs no explicit uninhibit: D-Bus inhibitions are
//! per-connection and the screensaver service drops them when the client
//! connection dies.
//!
//! Degrades with a log event when there is no session bus or no
//! org.freedesktop.ScreenSaver service on it (headless, some WMs) —
//! clipboard/mpris/pausemusic-style.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};

use tracing::{debug, info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

/// The screensaver-control seam. The real impl talks
/// org.freedesktop.ScreenSaver over the session bus; tests drive a mock.
#[async_trait::async_trait]
pub(crate) trait ScreensaverBackend: Send + Sync {
    /// Inhibit the screensaver; returns the inhibit cookie on success,
    /// None when the service is absent or the call fails (degraded).
    async fn inhibit(&self, app_name: &str, reason: &str) -> Option<u32>;
    /// Lift the inhibition, then SimulateUserActivity (upstream's
    /// unlocked-desktop guard — see module docs).
    async fn uninhibit_and_stimulate(&self, cookie: u32);
}

const APP_NAME: &str = "rust-connect";
const REASON: &str = "Phone is connected";

/// Wall-clock bound on a single `uninhibit_and_stimulate` awaited during
/// teardown. Session-bus calls are milliseconds; the bound protects
/// teardown latency (the registry awaits `on_disconnected`), not normal
/// operation. Test-only override lives on the plugin struct.
const DEFAULT_UNINHIBIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Per-device inhibit state machine. The generation counter is bumped on
/// EVERY connect and disconnect; an in-flight inhibit task that outlives
/// its connection generation sees a mismatch and self-cleans its own
/// cookie. The slot persists across disconnects so the counter does not
/// reset; only `Idle`/`Inhibiting`/`Inhibited` change. No servable
/// content is held here (cookie integer only).
struct InhibitSlot {
    generation: u64,
    state: InhibitState,
}

enum InhibitState {
    /// No live or in-flight inhibition for this device.
    Idle,
    /// A connect-time inhibit call is in flight on a spawned task.
    Inhibiting,
    /// The live cookie we hold for this device.
    Inhibited(u32),
}

pub struct ScreensaverInhibitPlugin {
    backend: StdRwLock<Option<Arc<dyn ScreensaverBackend>>>,
    /// device_id → per-device slot. Map lock is held only to clone an
    /// Arc; never held while a slot lock is held.
    slots: Arc<StdRwLock<HashMap<String, Arc<StdMutex<InhibitSlot>>>>>,
    /// Test-only override for the uninhibit await bound.
    #[cfg(test)]
    uninhibit_timeout: std::time::Duration,
}

impl Default for ScreensaverInhibitPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreensaverInhibitPlugin {
    pub fn new() -> Self {
        Self {
            backend: StdRwLock::new(None),
            slots: Arc::new(StdRwLock::new(HashMap::new())),
            #[cfg(test)]
            uninhibit_timeout: DEFAULT_UNINHIBIT_TIMEOUT,
        }
    }

    /// Connect the real session-bus backend. Called ONLY from the
    /// production entry point (bootstrap.rs create_state) — tests inject a
    /// mock with `with_backend`. Degrades with a log event when no session
    /// bus is reachable.
    pub async fn enable_session_backend(&self) {
        match ZbusScreensaverBackend::connect().await {
            Ok(backend) => {
                info!(
                    event = "screensaver_inhibit_backend_ready",
                    "Session screensaver-inhibit backend enabled"
                );
                if let Ok(mut guard) = self.backend.write() {
                    *guard = Some(Arc::new(backend));
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    event = "screensaver_inhibit_backend_unavailable",
                    "No session D-Bus for screensaver-inhibit. Screensaver will not be inhibited."
                );
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_backend(self, backend: Arc<dyn ScreensaverBackend>) -> Self {
        if let Ok(mut guard) = self.backend.write() {
            *guard = Some(backend);
        }
        self
    }

    #[cfg(test)]
    pub(crate) fn with_uninhibit_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.uninhibit_timeout = timeout;
        self
    }

    fn backend(&self) -> Option<Arc<dyn ScreensaverBackend>> {
        self.backend
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Test-visible cookie for a device. Contract preserved: Some(c) iff
    /// the slot's state is Inhibited(c).
    #[cfg(test)]
    pub(crate) fn cookie_for(&self, device_id: &str) -> Option<u32> {
        let slot = self
            .slots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .cloned()?;
        let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        match guard.state {
            InhibitState::Inhibited(c) => Some(c),
            _ => None,
        }
    }

    /// Map lock → clone Arc → drop map lock. Slot critical sections
    /// hold no awaits, so the map lock is never held while a slot is.
    fn slot_for(&self, device_id: &str) -> Arc<StdMutex<InhibitSlot>> {
        let map_read = self.slots.read().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map_read.get(device_id) {
            return existing.clone();
        }
        drop(map_read);
        let mut map_write = self.slots.write().unwrap_or_else(|e| e.into_inner());
        map_write
            .entry(device_id.to_string())
            .or_insert_with(|| {
                Arc::new(StdMutex::new(InhibitSlot {
                    generation: 0,
                    state: InhibitState::Idle,
                }))
            })
            .clone()
    }

    /// Connect-side critical section: bump generation, capture
    /// `my_gen`, transition state, note any prior cookie for release.
    /// No awaits inside the slot lock.
    fn begin_connect(&self, device_id: &str) -> (Arc<StdMutex<InhibitSlot>>, u64, Option<u32>) {
        let slot = self.slot_for(device_id);
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        guard.generation = guard.generation.wrapping_add(1);
        let my_gen = guard.generation;
        let old = match std::mem::replace(&mut guard.state, InhibitState::Inhibiting) {
            InhibitState::Inhibited(c) => Some(c),
            _ => None,
        };
        drop(guard);
        (slot, my_gen, old)
    }

    /// Disconnect-side critical section: bump generation, take the
    /// state, reset to Idle. No awaits inside the slot lock.
    fn begin_disconnect(&self, device_id: &str) -> (Arc<StdMutex<InhibitSlot>>, u64, InhibitState) {
        let slot = self.slot_for(device_id);
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        guard.generation = guard.generation.wrapping_add(1);
        let new_gen = guard.generation;
        let taken = std::mem::replace(&mut guard.state, InhibitState::Idle);
        drop(guard);
        (slot, new_gen, taken)
    }

    /// Awaited, bounded uninhibit. Log-and-continue on expiry or error
    /// (matches the `close_desktop_notification` style in
    /// `notification.rs`). Bounded so a wedged session bus cannot stall
    /// teardown latency indefinitely.
    async fn bounded_uninhibit(
        backend: &Arc<dyn ScreensaverBackend>,
        device_id: &str,
        cookie: u32,
        timeout: std::time::Duration,
    ) {
        match tokio::time::timeout(timeout, backend.uninhibit_and_stimulate(cookie)).await {
            Ok(()) => {
                info!(
                    device_id = %device_id,
                    cookie = cookie,
                    event = "screensaver_uninhibited",
                    "Phone disconnected, lifting screensaver inhibition"
                );
            }
            Err(_) => {
                warn!(
                    device_id = %device_id,
                    cookie = cookie,
                    event = "screensaver_uninhibit_timed_out",
                    "uninhibit_and_stimulate did not return within the bound; giving up"
                );
            }
        }
    }
}

#[async_trait::async_trait]
impl Plugin for ScreensaverInhibitPlugin {
    fn name(&self) -> &str {
        "screensaver-inhibit"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        // kdeconnect_screensaver_inhibit.json X-KdeConnect-SupportedPacketType: [].
        vec![]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // kdeconnect_screensaver_inhibit.json X-KdeConnect-OutgoingPacketType: [].
        vec![]
    }

    // Task 1.7: same pattern as clipboard.rs/mpris/mod.rs.
    fn is_backend_available(&self) -> bool {
        self.backend().is_some()
    }

    fn on_connected(&self, device_id: &str) -> Vec<Packet> {
        let Some(backend) = self.backend() else {
            debug!(
                device_id = %device_id,
                event = "screensaver_inhibit_no_backend",
                "Device connected but no screensaver backend; not inhibiting"
            );
            return vec![];
        };

        // Sync critical section: bump generation, capture my_gen,
        // transition to Inhibiting, note any prior cookie for release.
        let device_id = device_id.to_string();
        let (slot, my_gen, old_cookie) = self.begin_connect(&device_id);

        if let Some(old) = old_cookie {
            // Release a leaked prior cookie without blocking the
            // connect path; log-and-continue on failure. Best-effort.
            let backend = backend.clone();
            let device_id_release = device_id.clone();
            tokio::spawn(async move {
                Self::bounded_uninhibit(
                    &backend,
                    &device_id_release,
                    old,
                    DEFAULT_UNINHIBIT_TIMEOUT,
                )
                .await;
            });
        }

        // Spawn the inhibit task carrying (slot, my_gen). The
        // generation check is the whole fix: a task that outlives its
        // connection sees a bumped generation and releases its own
        // cookie instead of storing it.
        let device_id_spawn = device_id.clone();
        let backend_spawn = backend;
        let stale_bound = self.uninhibit_timeout();
        tokio::spawn(async move {
            let cookie = match backend_spawn.inhibit(APP_NAME, REASON).await {
                Some(c) => c,
                None => {
                    debug!(
                        device_id = %device_id_spawn,
                        event = "screensaver_inhibit_failed",
                        "Could not inhibit screensaver (no org.freedesktop.ScreenSaver service?)"
                    );
                    return;
                }
            };

            // Decide action under the lock, then release before any
            // await so the slot's MutexGuard does not cross it (Send).
            let release_now = {
                let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
                if guard.generation != my_gen {
                    true
                } else {
                    guard.state = InhibitState::Inhibited(cookie);
                    false
                }
            };

            if release_now {
                info!(
                    device_id = %device_id_spawn,
                    cookie = cookie,
                    my_gen = my_gen,
                    event = "screensaver_inhibit_stale_released",
                    "inhibit completed after disconnect; releasing its own cookie"
                );
                // Same bound as the disconnect path (review FINDINGS #1):
                // a wedged session bus must not park the stale task
                // forever. `screensaver_uninhibited` /
                // `screensaver_uninhibit_timed_out` from
                // bounded_uninhibit completes the audit's
                // every-cookie-accounted oracle.
                Self::bounded_uninhibit(&backend_spawn, &device_id_spawn, cookie, stale_bound)
                    .await;
                return;
            }
            info!(
                device_id = %device_id_spawn,
                cookie = cookie,
                event = "screensaver_inhibited",
                "Screensaver inhibited while phone is connected"
            );
        });

        vec![]
    }

    async fn on_disconnected(&self, device_id: &str) {
        // Sync critical section: bump generation, take state, reset.
        let (_slot, _new_gen, taken) = self.begin_disconnect(device_id);

        let device_id = device_id.to_string();
        match taken {
            InhibitState::Inhibited(cookie) => {
                let Some(backend) = self.backend() else {
                    // No backend → cannot uninhibit. The slot is
                    // already Idle; nothing more to do.
                    return;
                };
                // Awaited bounded uninhibit. Audit §C: the previous
                // spawn-and-return left the cookie held by the
                // session bus if uninhibit ever hung; we now bound
                // the wait so teardown latency stays predictable.
                Self::bounded_uninhibit(&backend, &device_id, cookie, self.uninhibit_timeout())
                    .await;
            }
            InhibitState::Inhibiting => {
                // The in-flight inhibit task sees the bumped
                // generation when it completes and self-cleans.
                // Nothing to do here.
            }
            InhibitState::Idle => {}
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        // No packet types — this should never be routed here.
        let _ = (device_id, packet);
        Ok(None)
    }
}

impl ScreensaverInhibitPlugin {
    fn uninhibit_timeout(&self) -> std::time::Duration {
        #[cfg(test)]
        {
            self.uninhibit_timeout
        }
        #[cfg(not(test))]
        {
            DEFAULT_UNINHIBIT_TIMEOUT
        }
    }
}

/// zbus session-bus backend for org.freedesktop.ScreenSaver.
pub(crate) struct ZbusScreensaverBackend {
    conn: zbus::Connection,
}

#[zbus::proxy(
    interface = "org.freedesktop.ScreenSaver",
    default_service = "org.freedesktop.ScreenSaver",
    default_path = "/ScreenSaver"
)]
trait ScreenSaver {
    fn inhibit(&self, app_name: &str, reason: &str) -> zbus::Result<u32>;
    fn un_inhibit(&self, cookie: u32) -> zbus::Result<()>;
    fn simulate_user_activity(&self) -> zbus::Result<()>;
}

impl ZbusScreensaverBackend {
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::session().await.map_err(|e| {
            crate::utils::errors::Error::Internal(format!("cannot connect to session D-Bus: {e}"))
        })?;
        Ok(Self { conn })
    }
}

#[async_trait::async_trait]
impl ScreensaverBackend for ZbusScreensaverBackend {
    async fn inhibit(&self, app_name: &str, reason: &str) -> Option<u32> {
        let proxy = ScreenSaverProxy::new(&self.conn).await.ok()?;
        match proxy.inhibit(app_name, reason).await {
            Ok(cookie) => Some(cookie),
            Err(e) => {
                debug!(error = %e, event = "screensaver_inhibit_call_failed", "Inhibit call failed");
                None
            }
        }
    }

    async fn uninhibit_and_stimulate(&self, cookie: u32) {
        let Ok(proxy) = ScreenSaverProxy::new(&self.conn).await else {
            return;
        };
        if let Err(e) = proxy.un_inhibit(cookie).await {
            debug!(error = %e, event = "screensaver_uninhibit_call_failed", "UnInhibit call failed");
        }
        // screensaverinhibitplugin.cpp:36-41 — restart the idle timer so
        // the desktop doesn't sit unlocked.
        let _ = proxy.simulate_user_activity().await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct MockBackend {
        inhibits: StdRwLock<Vec<(String, String)>>,
        uninhibits: StdRwLock<Vec<u32>>,
        next_cookie: AtomicUsize,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                inhibits: StdRwLock::new(Vec::new()),
                uninhibits: StdRwLock::new(Vec::new()),
                next_cookie: AtomicUsize::new(100),
            }
        }
    }

    #[async_trait::async_trait]
    impl ScreensaverBackend for MockBackend {
        async fn inhibit(&self, app_name: &str, reason: &str) -> Option<u32> {
            self.inhibits
                .write()
                .unwrap()
                .push((app_name.to_string(), reason.to_string()));
            Some(self.next_cookie.fetch_add(1, Ordering::SeqCst) as u32)
        }

        async fn uninhibit_and_stimulate(&self, cookie: u32) {
            self.uninhibits.write().unwrap().push(cookie);
        }
    }

    /// Gated fake: `inhibit` parks until the test releases it. Used to
    /// drive deterministic interleavings between connect-time inhibit and
    /// disconnect-time uninhibit (audit §C, PR #40 review).
    struct GatedBackend {
        inhibits: StdRwLock<Vec<(String, String)>>,
        uninhibits: StdRwLock<Vec<u32>>,
        next_cookie: AtomicUsize,
        in_flight_cookie: StdRwLock<Option<u32>>,
        gate: Notify,
    }

    impl GatedBackend {
        fn new() -> Self {
            Self {
                inhibits: StdRwLock::new(Vec::new()),
                uninhibits: StdRwLock::new(Vec::new()),
                next_cookie: AtomicUsize::new(100),
                in_flight_cookie: StdRwLock::new(None),
                gate: Notify::new(),
            }
        }

        fn release(&self) {
            self.gate.notify_one();
        }

        /// Cookie the parked inhibit will return when released. Recorded
        /// by the task itself, so the value is stable as long as the
        /// task has reached `notified().await`.
        fn in_flight_cookie(&self) -> Option<u32> {
            *self.in_flight_cookie.read().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl ScreensaverBackend for GatedBackend {
        async fn inhibit(&self, app_name: &str, reason: &str) -> Option<u32> {
            self.inhibits
                .write()
                .unwrap()
                .push((app_name.to_string(), reason.to_string()));
            let cookie = self.next_cookie.fetch_add(1, Ordering::SeqCst) as u32;
            *self.in_flight_cookie.write().unwrap() = Some(cookie);
            self.gate.notified().await;
            Some(cookie)
        }

        async fn uninhibit_and_stimulate(&self, cookie: u32) {
            self.uninhibits.write().unwrap().push(cookie);
        }
    }

    /// Gated inhibit + cancel-detecting hanging uninhibit. The inhibit
    /// parks on a Notify gate (drives a stale task deterministically);
    /// the uninhibit records its start, installs a drop-guard, then
    /// parks forever. The drop-guard flips `cancelled` only when the
    /// future is DROPPED — which a timeout-wrapped await does on expiry
    /// and an unbounded await never does. That difference is the
    /// assertion: every uninhibit the plugin awaits must be bounded
    /// (audit §C family: a wedged session bus must not park a task
    /// forever, wherever in the lifecycle the call happens).
    struct GatedHangBackend {
        inhibits: StdRwLock<Vec<(String, String)>>,
        next_cookie: AtomicUsize,
        in_flight_cookie: StdRwLock<Option<u32>>,
        gate: Notify,
        uninhibit_started: StdRwLock<Vec<u32>>,
        uninhibit_cancelled: AtomicBool,
    }

    impl GatedHangBackend {
        fn new() -> Self {
            Self {
                inhibits: StdRwLock::new(Vec::new()),
                next_cookie: AtomicUsize::new(100),
                in_flight_cookie: StdRwLock::new(None),
                gate: Notify::new(),
                uninhibit_started: StdRwLock::new(Vec::new()),
                uninhibit_cancelled: AtomicBool::new(false),
            }
        }

        fn release(&self) {
            self.gate.notify_one();
        }

        fn in_flight_cookie(&self) -> Option<u32> {
            *self.in_flight_cookie.read().unwrap()
        }

        fn started(&self) -> Vec<u32> {
            self.uninhibit_started.read().unwrap().clone()
        }

        fn cancelled(&self) -> bool {
            self.uninhibit_cancelled.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ScreensaverBackend for GatedHangBackend {
        async fn inhibit(&self, app_name: &str, reason: &str) -> Option<u32> {
            self.inhibits
                .write()
                .unwrap()
                .push((app_name.to_string(), reason.to_string()));
            let cookie = self.next_cookie.fetch_add(1, Ordering::SeqCst) as u32;
            *self.in_flight_cookie.write().unwrap() = Some(cookie);
            self.gate.notified().await;
            Some(cookie)
        }

        async fn uninhibit_and_stimulate(&self, cookie: u32) {
            self.uninhibit_started.write().unwrap().push(cookie);
            struct CancelGuard<'a>(&'a AtomicBool);
            impl Drop for CancelGuard<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _guard = CancelGuard(&self.uninhibit_cancelled);
            std::future::pending::<()>().await;
        }
    }

    /// Hanging fake: `uninhibit_and_stimulate` parks forever. Drives the
    /// bounded-uninhibit timeout test (audit §C: awaited bounded cleanup,
    /// not a spawn).
    struct HangingBackend {
        inhibits: StdRwLock<Vec<(String, String)>>,
        #[allow(dead_code)]
        uninhibits: StdRwLock<Vec<u32>>,
        next_cookie: AtomicUsize,
    }

    impl HangingBackend {
        fn new() -> Self {
            Self {
                inhibits: StdRwLock::new(Vec::new()),
                uninhibits: StdRwLock::new(Vec::new()),
                next_cookie: AtomicUsize::new(100),
            }
        }
    }

    #[async_trait::async_trait]
    impl ScreensaverBackend for HangingBackend {
        async fn inhibit(&self, app_name: &str, reason: &str) -> Option<u32> {
            self.inhibits
                .write()
                .unwrap()
                .push((app_name.to_string(), reason.to_string()));
            Some(self.next_cookie.fetch_add(1, Ordering::SeqCst) as u32)
        }

        async fn uninhibit_and_stimulate(&self, _cookie: u32) {
            // Park forever; the production code must bound this with a
            // timeout so teardown latency is not held hostage by a
            // wedged session bus.
            std::future::pending::<()>().await;
        }
    }

    async fn wait_until<F: FnMut() -> bool>(mut cond: F) -> bool {
        for _ in 0..80 {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
    }

    #[tokio::test]
    async fn test_screensaver_inhibit_name_and_capabilities() {
        let plugin = ScreensaverInhibitPlugin::new();
        assert_eq!(plugin.name(), "screensaver-inhibit");
        assert!(plugin.incoming_capabilities().is_empty());
        assert!(plugin.outgoing_capabilities().is_empty());
    }

    /// Task 1.7: is_backend_available must reflect the injected backend,
    /// not the Plugin trait's default `true`. Same pattern as
    /// clipboard.rs/mpris/mod.rs. This plugin advertises no incoming
    /// capabilities of its own, so /api/v1/tools never surfaces it either
    /// way — the override is still correct on principle and future-proofs
    /// against that changing.
    #[tokio::test]
    async fn test_is_backend_available_reflects_injected_backend() {
        let plugin = ScreensaverInhibitPlugin::new();
        assert!(!plugin.is_backend_available());

        let backend = Arc::new(MockBackend::new());
        let plugin = plugin.with_backend(backend);
        assert!(plugin.is_backend_available());
    }

    #[tokio::test]
    async fn test_connect_inhibits_and_stores_cookie() {
        let backend = Arc::new(MockBackend::new());
        let plugin = ScreensaverInhibitPlugin::new().with_backend(backend.clone());
        let packets = plugin.on_connected("device1");
        assert!(packets.is_empty(), "no packets are ever sent");
        assert!(wait_until(|| plugin.cookie_for("device1").is_some()).await);
        let inhibits = backend.inhibits.read().unwrap().clone();
        assert_eq!(
            inhibits,
            vec![("rust-connect".to_string(), "Phone is connected".to_string())]
        );
    }

    #[tokio::test]
    async fn test_disconnect_uninhibits_with_stored_cookie() {
        let backend = Arc::new(MockBackend::new());
        let plugin = ScreensaverInhibitPlugin::new().with_backend(backend.clone());
        plugin.on_connected("device1");
        assert!(wait_until(|| plugin.cookie_for("device1").is_some()).await);
        let cookie = plugin.cookie_for("device1").unwrap();

        plugin.on_disconnected("device1").await;
        assert!(wait_until(|| !backend.uninhibits.read().unwrap().is_empty()).await);
        assert_eq!(backend.uninhibits.read().unwrap().clone(), vec![cookie]);
        assert!(plugin.cookie_for("device1").is_none());
    }

    #[tokio::test]
    async fn test_disconnect_without_connect_is_noop() {
        let backend = Arc::new(MockBackend::new());
        let plugin = ScreensaverInhibitPlugin::new().with_backend(backend.clone());
        plugin.on_disconnected("device1").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(backend.uninhibits.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_cookies_are_per_device() {
        let backend = Arc::new(MockBackend::new());
        let plugin = ScreensaverInhibitPlugin::new().with_backend(backend.clone());
        plugin.on_connected("device1");
        plugin.on_connected("device2");
        assert!(
            wait_until(|| {
                plugin.cookie_for("device1").is_some() && plugin.cookie_for("device2").is_some()
            })
            .await
        );
        let c1 = plugin.cookie_for("device1").unwrap();
        let c2 = plugin.cookie_for("device2").unwrap();
        assert_ne!(c1, c2);

        // device1 leaving lifts only its own inhibition.
        plugin.on_disconnected("device1").await;
        assert!(wait_until(|| !backend.uninhibits.read().unwrap().is_empty()).await);
        assert_eq!(backend.uninhibits.read().unwrap().clone(), vec![c1]);
        assert!(plugin.cookie_for("device2").is_some());
    }

    #[tokio::test]
    async fn test_no_backend_degrades_cleanly() {
        let plugin = ScreensaverInhibitPlugin::new();
        assert!(plugin.on_connected("device1").is_empty());
        plugin.on_disconnected("device1").await; // must not panic
    }

    /// R1 gate test (audit §C, PR #40 review): if `on_disconnected`
    /// arrives while the inhibit call is in flight, the cookie the call
    /// subsequently returns must still be released. Pre-fix: the
    /// disconnect early-returns (no stored cookie → nothing to do) and
    /// the task then stores the cookie, which stays orphaned until the
    /// next full connect/disconnect cycle. The screen would never lock
    /// again. Post-fix: the disconnect bumps the slot's generation; the
    /// in-flight inhibit task sees the bump and self-cleans its own
    /// cookie rather than storing it. The cookie is released, and
    /// `cookie_for` stays `None` — the slot never records the orphan.
    #[tokio::test]
    async fn test_disconnect_before_cookie_stored_still_uninhibits() {
        let backend = Arc::new(GatedBackend::new());
        let plugin = ScreensaverInhibitPlugin::new().with_backend(backend.clone());

        // Connect. The inhibit task is spawned; the fake parks on the
        // gate, so no cookie is stored yet.
        plugin.on_connected("device1");
        // Give the spawn a tick to enter the inhibit future.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            plugin.cookie_for("device1").is_none(),
            "gate should hold the inhibit task open before the cookie is returned"
        );

        // Disconnect. The slot bumps its generation; the gated
        // inhibit is still parked with the same cookie fetched.
        plugin.on_disconnected("device1").await;

        // Release the in-flight inhibit. With the fix in place, the
        // task sees the bumped generation and self-cleans: it
        // releases the cookie itself instead of storing it.
        let issued = backend
            .in_flight_cookie()
            .expect("the gated inhibit task should have fetched its cookie");
        backend.release();
        assert!(
            wait_until(|| backend.uninhibits.read().unwrap().contains(&issued)).await,
            "the cookie issued after disconnect must be released by the stale inhibit task"
        );
        assert!(
            plugin.cookie_for("device1").is_none(),
            "no cookie should be stored after a stale inhibit self-cleans"
        );
    }

    /// Audit §C, sibling leak: `notify_connected` fires on EVERY link
    /// replace, so a second connect with no disconnect overwrites the
    /// stored cookie via `insert` and leaks the first one. The slot
    /// state machine must release the previous cookie before issuing a
    /// fresh one.
    #[tokio::test]
    async fn test_connect_again_without_disconnect_releases_old_cookie() {
        let backend = Arc::new(GatedBackend::new());
        let plugin = ScreensaverInhibitPlugin::new().with_backend(backend.clone());

        // First connect: park inhibit 1; capture cookie 1; release.
        plugin.on_connected("device1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let cookie1 = backend
            .in_flight_cookie()
            .expect("first inhibit task should have fetched its cookie");
        backend.release();
        assert!(
            wait_until(|| plugin.cookie_for("device1") == Some(cookie1)).await,
            "first cookie should be stored once inhibit completes"
        );

        // Second connect without disconnect: park inhibit 2; release.
        // The slot must release cookie 1 before issuing cookie 2, and
        // store cookie 2.
        plugin.on_connected("device1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let cookie2 = backend
            .in_flight_cookie()
            .expect("second inhibit task should have fetched its cookie");
        backend.release();
        assert!(
            wait_until(|| plugin.cookie_for("device1") == Some(cookie2)).await,
            "second cookie should be the current one once its inhibit completes"
        );
        assert!(
            wait_until(|| backend.uninhibits.read().unwrap().contains(&cookie1)).await,
            "first cookie must be released by the second connect (no-disconnect leak)"
        );
    }

    /// Deterministic interleaving with the gated fake: a stale inhibit
    /// task (in flight at disconnect time) must self-clean its own
    /// cookie when it sees a bumped generation. No leaked cookies.
    #[tokio::test]
    async fn test_stale_inhibit_self_cleans_after_disconnect() {
        let backend = Arc::new(GatedBackend::new());
        let plugin = ScreensaverInhibitPlugin::new().with_backend(backend.clone());

        // connect1: task parked
        plugin.on_connected("device1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let cookie1 = backend
            .in_flight_cookie()
            .expect("first inhibit task should have fetched its cookie");
        backend.release();
        assert!(
            wait_until(|| plugin.cookie_for("device1") == Some(cookie1)).await,
            "connect1's cookie should be stored"
        );

        // disconnect: awaited uninhibit on cookie1
        plugin.on_disconnected("device1").await;
        assert!(
            wait_until(|| backend.uninhibits.read().unwrap().contains(&cookie1)).await,
            "cookie1 must be released on disconnect"
        );
        assert!(plugin.cookie_for("device1").is_none());

        // Now the interleaving that the current code drops: connect2
        // starts an inhibit task, which we then leave in flight when
        // disconnect fires, then release the gate.
        plugin.on_connected("device1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let cookie2 = backend
            .in_flight_cookie()
            .expect("second inhibit task should have fetched its cookie");
        // disconnect2: today's code removes no cookie (slot is
        // Inhibiting, not Inhibited) and forgets about cookie2.
        plugin.on_disconnected("device1").await;
        // Release the parked inhibit. Stale task: must self-clean.
        backend.release();
        assert!(
            wait_until(|| backend.uninhibits.read().unwrap().contains(&cookie2)).await,
            "stale connect2 task must release cookie2 itself after disconnect bumps generation"
        );
        assert!(plugin.cookie_for("device1").is_none());
    }

    /// Audit §C: awaited bounded uninhibit. A wedged session bus must
    /// not stall `on_disconnected` forever; the registry awaits this.
    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn test_on_disconnect_bounds_uninhibit_under_hang() {
        let backend = Arc::new(HangingBackend::new());
        // Tight override so the test stays in suite budget; production
        // default is 5s.
        let bound = std::time::Duration::from_millis(200);
        let plugin = ScreensaverInhibitPlugin::new()
            .with_backend(backend.clone())
            .with_uninhibit_timeout(bound);

        plugin.on_connected("device1");
        assert!(wait_until(|| plugin.cookie_for("device1").is_some()).await);

        let started = std::time::Instant::now();
        let disconnect = tokio::time::timeout(
            bound + std::time::Duration::from_millis(500),
            plugin.on_disconnected("device1"),
        );
        let completed = disconnect.await;
        assert!(
            completed.is_ok(),
            "on_disconnected must return within the bound even if uninhibit hangs"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < bound + std::time::Duration::from_millis(250),
            "disconnect must return promptly even if uninhibit parks (took {elapsed:?})"
        );
        assert!(plugin.cookie_for("device1").is_none());
    }

    /// FINDINGS #1 (review round): the stale-task self-clean must be
    /// bounded, not just the disconnect path. A stale inhibit task whose
    /// `uninhibit_and_stimulate` hangs must not park the spawned task
    /// forever — the await has to be dropped at the bound, exactly like
    /// `on_disconnected`'s awaited cleanup. Red on the branch under
    /// review: the stale path awaited the backend call with no timeout.
    #[tokio::test]
    async fn test_stale_task_self_clean_is_bounded_under_hang() {
        let backend = Arc::new(GatedHangBackend::new());
        let bound = std::time::Duration::from_millis(150);
        let plugin = ScreensaverInhibitPlugin::new()
            .with_backend(backend.clone())
            .with_uninhibit_timeout(bound);

        // Connect; the inhibit task parks on the gate before returning
        // its cookie.
        plugin.on_connected("device1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let issued = backend
            .in_flight_cookie()
            .expect("the gated inhibit task should have fetched its cookie");

        // Disconnect while the inhibit is in flight: the generation
        // bumps, the slot goes Idle, nothing is awaited on this path.
        plugin.on_disconnected("device1").await;

        // Release: the task is stale and must self-clean — into a
        // backend whose uninhibit hangs.
        backend.release();
        assert!(
            wait_until(|| backend.started().contains(&issued)).await,
            "the stale task must call uninhibit for its own cookie"
        );
        assert!(
            wait_until(|| backend.cancelled()).await,
            "the stale task's uninhibit must be dropped at the bound, not awaited forever"
        );
        assert!(
            plugin.cookie_for("device1").is_none(),
            "no cookie stored: the stale task self-cleaned"
        );
    }
}
