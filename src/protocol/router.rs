//! Packet routing to registered handlers
//!
//! Single Responsibility: Route packets to handlers by packet type.
//! Does NOT handle packet parsing or network I/O.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;

use futures::FutureExt;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::{Error, Result};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

type PacketHandler =
    Arc<dyn Fn(&str, Packet) -> BoxFuture<'static, Result<Option<Vec<Packet>>>> + Send + Sync>;

pub struct PacketRouter {
    handlers: Arc<RwLock<HashMap<String, Vec<PacketHandler>>>>,
    default_handlers: Arc<RwLock<Vec<PacketHandler>>>,
}

impl PacketRouter {
    pub fn new() -> Self {
        Self {
            handlers: Arc::new(RwLock::new(HashMap::new())),
            default_handlers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn register<F, Fut>(&self, packet_type: &str, handler: F)
    where
        F: Fn(&str, Packet) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<Vec<Packet>>>> + Send + 'static,
    {
        let mut handlers = self.handlers.write().await;
        debug!(
            packet_type = %packet_type,
            event = "handler_registered",
            "Registered packet handler"
        );
        let handler_arc: PacketHandler =
            Arc::new(move |device_id, packet| Box::pin(handler(device_id, packet)));
        handlers
            .entry(packet_type.to_string())
            .or_default()
            .push(handler_arc);
    }

    pub async fn register_default<F, Fut>(&self, handler: F)
    where
        F: Fn(&str, Packet) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<Vec<Packet>>>> + Send + 'static,
    {
        let mut default = self.default_handlers.write().await;
        debug!(
            event = "default_handler_registered",
            "Registered default packet handler"
        );
        let handler_arc: PacketHandler =
            Arc::new(move |device_id, packet| Box::pin(handler(device_id, packet)));
        default.push(handler_arc);
    }

    pub async fn unregister(&self, packet_type: &str) -> bool {
        let mut handlers = self.handlers.write().await;
        handlers.remove(packet_type).is_some()
    }

    pub async fn route(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        let handlers_lock = self.handlers.read().await;

        if let Some(handlers) = handlers_lock.get(&packet.packet_type) {
            debug!(
                packet_type = %packet.packet_type,
                handler_count = handlers.len(),
                event = "packet_routed",
                "Routed packet to handlers"
            );

            let handlers_clone = handlers.clone();
            drop(handlers_lock);

            return Self::call_handlers(handlers_clone, device_id, packet).await;
        }

        drop(handlers_lock);

        let default_lock = self.default_handlers.read().await;
        if !default_lock.is_empty() {
            debug!(
                packet_type = %packet.packet_type,
                handler_count = default_lock.len(),
                event = "packet_routed_default",
                "Routed packet to default handlers"
            );
            let handlers_clone = default_lock.clone();
            drop(default_lock);

            return Self::call_handlers(handlers_clone, device_id, packet).await;
        }

        warn!(
            packet_type = %packet.packet_type,
            event = "no_handler",
            "No handler for packet type"
        );
        Err(Error::NoPluginForPacketType(packet.packet_type))
    }

    async fn call_handlers(
        handlers: Vec<PacketHandler>,
        device_id: &str,
        packet: Packet,
    ) -> Result<Option<Vec<Packet>>> {
        async fn run_one(
            handler: &PacketHandler,
            device_id: &str,
            pkt: Packet,
            all_responses: &mut Vec<Packet>,
            first_error: &mut Option<Error>,
            any_ok: &mut bool,
        ) {
            let packet_type = pkt.packet_type.clone();
            // A panicking plugin must not unwind out of route(): the panic
            // would kill the connection task's packet loop and sever a
            // healthy link. Catch per-handler so siblings still run and the
            // loop keeps going; the panic surfaces as a handler failure.
            // Building the future inside the async block puts a SYNCHRONOUS
            // panic in the handler call itself behind the same boundary.
            let result = AssertUnwindSafe(async move { handler(device_id, pkt).await })
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(Some(mut responses))) => {
                    *any_ok = true;
                    all_responses.append(&mut responses);
                }
                Ok(Ok(None)) => {
                    *any_ok = true;
                }
                Ok(Err(e)) => {
                    // Fan-out is per-handler fault-tolerant: with more than
                    // one handler on a packet type (telephony + pausemusic
                    // both consume kdeconnect.telephony), a failing plugin
                    // must not starve its siblings or drop responses
                    // already collected. The failure is surfaced in the
                    // log, and — only when NO handler succeeded at all —
                    // also as route()'s Err, so "every handler failed"
                    // stays distinguishable from "no handler had a
                    // response". A sibling that succeeded with Ok(None)
                    // still counts as processed.
                    error!(
                        device_id = %device_id,
                        error = %e,
                        event = "plugin_error",
                        "Plugin returned error handling packet"
                    );
                    if first_error.is_none() {
                        *first_error = Some(e);
                    }
                }
                Err(panic) => {
                    let panic_msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic payload".to_string());
                    error!(
                        device_id = %device_id,
                        packet_type = %packet_type,
                        panic = %panic_msg,
                        event = "plugin_panic",
                        "Plugin panicked handling packet"
                    );
                    if first_error.is_none() {
                        *first_error = Some(Error::Internal(format!(
                            "Plugin panicked handling {}: {}",
                            packet_type, panic_msg
                        )));
                    }
                }
            }
        }

        let mut all_responses = Vec::new();
        let mut first_error: Option<Error> = None;
        let mut any_ok = false;

        // Since Packet needs to be passed by value and there could be
        // multiple handlers, we clone it for all but the last.
        let mut iter = handlers.into_iter();
        let last_handler = iter.next_back();
        for handler in iter {
            run_one(
                &handler,
                device_id,
                packet.clone(),
                &mut all_responses,
                &mut first_error,
                &mut any_ok,
            )
            .await;
        }
        if let Some(handler) = last_handler {
            run_one(
                &handler,
                device_id,
                packet,
                &mut all_responses,
                &mut first_error,
                &mut any_ok,
            )
            .await;
        }

        if !all_responses.is_empty() {
            Ok(Some(all_responses))
        } else if !any_ok {
            match first_error {
                Some(e) => Err(e),
                None => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    pub async fn registered_types(&self) -> Vec<String> {
        let handlers = self.handlers.read().await;
        handlers.keys().cloned().collect()
    }

    pub async fn has_handler(&self, packet_type: &str) -> bool {
        let handlers = self.handlers.read().await;
        handlers.contains_key(packet_type)
    }
}

impl Default for PacketRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_register_and_route() {
        let router = PacketRouter::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        router
            .register("kdeconnect.ping", move |_device_id, packet| {
                let counter_clone = counter_clone.clone();
                async move {
                    assert_eq!(packet.packet_type, "kdeconnect.ping");
                    counter_clone.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                }
            })
            .await;

        let packet = Packet::ping();
        let _ = router
            .route("test", packet)
            .await
            .expect("Value expected to be present");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_route_unknown_type() {
        let router = PacketRouter::new();
        let packet = Packet::new("kdeconnect.unknown".to_string(), serde_json::json!({}));
        let result = router.route("test", packet).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_failing_handler_does_not_starve_siblings() {
        // With two handlers on one packet type (telephony + pausemusic both
        // consume kdeconnect.telephony), a failing handler must not skip
        // its sibling or drop the sibling's responses.
        let router = PacketRouter::new();
        router
            .register("kdeconnect.telephony", |_device_id, _packet| async move {
                Err(Error::Internal("boom".to_string()))
            })
            .await;
        router
            .register("kdeconnect.telephony", |_device_id, _packet| async move {
                Ok(Some(vec![Packet::ping()]))
            })
            .await;

        let packet = Packet::new("kdeconnect.telephony".to_string(), serde_json::json!({}));
        let responses = router
            .route("test", packet)
            .await
            .expect("a sibling handler's failure must not fail the route")
            .expect("the healthy sibling's responses must survive");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].packet_type, "kdeconnect.ping");
    }

    #[tokio::test]
    async fn test_total_handler_failure_still_errors() {
        // Sibling tolerance must not make "every handler failed"
        // indistinguishable from "no handler had a response": with nothing
        // usable collected, route() returns the first error.
        let router = PacketRouter::new();
        router
            .register("kdeconnect.telephony", |_device_id, _packet| async move {
                Err(Error::Internal("boom".to_string()))
            })
            .await;
        let packet = Packet::new("kdeconnect.telephony".to_string(), serde_json::json!({}));
        assert!(router.route("test", packet).await.is_err());

        // Same with TWO failing handlers — siblings ran, all failed.
        let router = PacketRouter::new();
        for _ in 0..2 {
            router
                .register("kdeconnect.telephony", |_device_id, _packet| async move {
                    Err(Error::Internal("boom".to_string()))
                })
                .await;
        }
        let packet = Packet::new("kdeconnect.telephony".to_string(), serde_json::json!({}));
        assert!(router.route("test", packet).await.is_err());
    }

    #[tokio::test]
    async fn test_error_plus_successful_none_sibling_is_ok_none() {
        // A sibling that processed the packet and returned Ok(None) counts
        // as success: one failure + one success is a PARTIAL failure —
        // logged, not an Err.
        let router = PacketRouter::new();
        router
            .register("kdeconnect.telephony", |_device_id, _packet| async move {
                Err(Error::Internal("boom".to_string()))
            })
            .await;
        router
            .register("kdeconnect.telephony", |_device_id, _packet| async move {
                Ok(None)
            })
            .await;
        let packet = Packet::new("kdeconnect.telephony".to_string(), serde_json::json!({}));
        let result = router
            .route("test", packet)
            .await
            .expect("a processed-but-empty sibling makes the route succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_panicking_handler_does_not_unwind_route() {
        // A panicking plugin must be contained per-handler: the route
        // returns normally and healthy siblings still run.
        let router = PacketRouter::new();
        router
            .register("kdeconnect.telephony", |_device_id, _packet| async move {
                panic!("deliberate test panic");
            })
            .await;
        router
            .register("kdeconnect.telephony", |_device_id, _packet| async move {
                Ok(Some(vec![Packet::ping()]))
            })
            .await;

        let packet = Packet::new("kdeconnect.telephony".to_string(), serde_json::json!({}));
        let responses = router
            .route("test", packet)
            .await
            .expect("a panicking handler must not fail the route")
            .expect("the healthy sibling's responses must survive a panicking sibling");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].packet_type, "kdeconnect.ping");
    }

    #[tokio::test]
    async fn test_panicking_handler_alone_surfaces_as_error() {
        // With no healthy sibling, the panic is the route's error — an Err,
        // never an unwind out of route().
        let router = PacketRouter::new();
        router
            .register("kdeconnect.telephony", |_device_id, _packet| async move {
                panic!("deliberate test panic");
            })
            .await;
        let packet = Packet::new("kdeconnect.telephony".to_string(), serde_json::json!({}));
        let err = router
            .route("test", packet)
            .await
            .expect_err("a lone panicking handler must surface as an error");
        assert!(err.to_string().contains("panicked"), "{err}");
    }

    #[tokio::test]
    async fn test_default_handler() {
        let router = PacketRouter::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        router
            .register_default(move |_device_id, _packet| {
                let counter_clone = counter_clone.clone();
                async move {
                    counter_clone.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                }
            })
            .await;

        let packet = Packet::new("kdeconnect.anything".to_string(), serde_json::json!({}));
        let _ = router
            .route("test", packet)
            .await
            .expect("Value expected to be present");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_unregister() {
        let router = PacketRouter::new();
        router
            .register("kdeconnect.ping", |_device_id, _packet| async { Ok(None) })
            .await;

        assert!(router.unregister("kdeconnect.ping").await);
        assert!(!router.unregister("kdeconnect.ping").await);

        let packet = Packet::ping();
        assert!(router.route("test", packet).await.is_err());
    }

    #[tokio::test]
    async fn test_registered_types() {
        let router = PacketRouter::new();
        router
            .register("kdeconnect.ping", |_device_id, _packet| async { Ok(None) })
            .await;
        router
            .register("kdeconnect.pair", |_device_id, _packet| async { Ok(None) })
            .await;

        let mut types = router.registered_types().await;
        types.sort();
        assert_eq!(types, vec!["kdeconnect.pair", "kdeconnect.ping"]);
    }

    #[tokio::test]
    async fn test_has_handler() {
        let router = PacketRouter::new();
        assert!(!router.has_handler("kdeconnect.ping").await);
        router
            .register("kdeconnect.ping", |_device_id, _packet| async { Ok(None) })
            .await;
        assert!(router.has_handler("kdeconnect.ping").await);
    }

    #[tokio::test]
    async fn test_specific_takes_precedence_over_default() {
        let router = PacketRouter::new();
        let specific_counter = Arc::new(AtomicUsize::new(0));
        let default_counter = Arc::new(AtomicUsize::new(0));
        let sc = specific_counter.clone();
        let dc = default_counter.clone();

        router
            .register("kdeconnect.ping", move |_device_id, _packet| {
                let sc = sc.clone();
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                }
            })
            .await;
        router
            .register_default(move |_device_id, _packet| {
                let dc = dc.clone();
                async move {
                    dc.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                }
            })
            .await;

        let packet = Packet::ping();
        let _ = router
            .route("test", packet)
            .await
            .expect("Value expected to be present");
        assert_eq!(specific_counter.load(Ordering::SeqCst), 1);
        assert_eq!(default_counter.load(Ordering::SeqCst), 0);
    }
}
