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
use std::sync::{Arc, RwLock as StdRwLock};

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

pub struct ScreensaverInhibitPlugin {
    backend: StdRwLock<Option<Arc<dyn ScreensaverBackend>>>,
    /// device_id → inhibit cookie (upstream: one per-device plugin
    /// instance = one cookie per device).
    cookies: Arc<StdRwLock<HashMap<String, u32>>>,
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
            cookies: Arc::new(StdRwLock::new(HashMap::new())),
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

    fn backend(&self) -> Option<Arc<dyn ScreensaverBackend>> {
        self.backend
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Test-visible cookie for a device.
    #[cfg(test)]
    pub(crate) fn cookie_for(&self, device_id: &str) -> Option<u32> {
        self.cookies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .copied()
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

        let device_id = device_id.to_string();
        let cookies = self.cookies.clone();
        tokio::spawn(async move {
            match backend.inhibit(APP_NAME, REASON).await {
                Some(cookie) => {
                    info!(
                        device_id = %device_id,
                        cookie = cookie,
                        event = "screensaver_inhibited",
                        "Screensaver inhibited while phone is connected"
                    );
                    cookies
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(device_id, cookie);
                }
                None => {
                    debug!(
                        device_id = %device_id,
                        event = "screensaver_inhibit_failed",
                        "Could not inhibit screensaver (no org.freedesktop.ScreenSaver service?)"
                    );
                }
            }
        });

        vec![]
    }

    fn on_disconnected(&self, device_id: &str) {
        let cookie = self
            .cookies
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id);
        let Some(cookie) = cookie else {
            return;
        };
        let Some(backend) = self.backend() else {
            return;
        };

        info!(
            device_id = %device_id,
            cookie = cookie,
            event = "screensaver_uninhibited",
            "Phone disconnected, lifting screensaver inhibition"
        );
        tokio::spawn(async move {
            backend.uninhibit_and_stimulate(cookie).await;
        });
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        // No packet types — this should never be routed here.
        let _ = (device_id, packet);
        Ok(None)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

        plugin.on_disconnected("device1");
        assert!(wait_until(|| !backend.uninhibits.read().unwrap().is_empty()).await);
        assert_eq!(backend.uninhibits.read().unwrap().clone(), vec![cookie]);
        assert!(plugin.cookie_for("device1").is_none());
    }

    #[tokio::test]
    async fn test_disconnect_without_connect_is_noop() {
        let backend = Arc::new(MockBackend::new());
        let plugin = ScreensaverInhibitPlugin::new().with_backend(backend.clone());
        plugin.on_disconnected("device1");
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
        plugin.on_disconnected("device1");
        assert!(wait_until(|| !backend.uninhibits.read().unwrap().is_empty()).await);
        assert_eq!(backend.uninhibits.read().unwrap().clone(), vec![c1]);
        assert!(plugin.cookie_for("device2").is_some());
    }

    #[tokio::test]
    async fn test_no_backend_degrades_cleanly() {
        let plugin = ScreensaverInhibitPlugin::new();
        assert!(plugin.on_connected("device1").is_empty());
        plugin.on_disconnected("device1"); // must not panic
    }
}
