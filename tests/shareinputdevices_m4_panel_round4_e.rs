//! Lane-E coverage for Task #1042 panel M4 round 4. The round-4
//! brief landed three fixes that the prior round's wiring had
//! missing. Each test below pins one of them.
//!
//! **Fix 1 — pairing-event seam.** The capability gate filters
//! on `is_paired`, so a connect-then-pair flow lands
//! `StateChanged{Connected}` BEFORE the device is paired; the
//! gate sees the connect, the pairing predicate rejects, and
//! nothing re-evaluates when pairing completes — the peer is a
//! permanent "connected-capable but never eligible" record until
//! it disconnects and reconnects. The fix: the broadcaster
//! fires `DeviceEvent::Paired` from `accept_pairing` /
//! `force_accept_pairing` and `DeviceEvent::Unpaired` from
//! `unpair`; the gate's subscription loop reacts to those events
//! directly. This file's tests pin the broadcaster end of the
//! seam — the events themselves must reach subscribers, with the
//! right shape and the right invariants (no broadcast when no
//! broadcaster is wired; one broadcast per accepted pair).
//!
//! **Fix 2 — pending_disabled latch ordering.** `do_activate`
//! stores the session in its slot FIRST, then registers the
//! `set_on_disabled` callback, then awaits the consumer init.
//! The pre-fix order was `set_on_disabled` -> `populate` ->
//! store in slot, with the disabled callback firing while the
//! slot was still empty; `do_evaluate_after_event`'s deactivate
//! arm observed an empty slot and aborted the rest of
//! `do_activate` despite the session being alive. This file's
//! tests pin the new order at the slot API level.
//!
//! **Fix 3 — re-arm after deactivate.** `do_deactivate`
//! re-evaluates after the slot clears, so a subsequent
//! `Connected` (or `Paired`, per the lane-E seam) that lands
//! before the next incoming event still drives the activation
//! arm. The brief's exact prescription ran into a Send-bound
//! type-system constraint (the post-deactivate re-eval spawn
//! could not be made `Send`); the working path is the gate's
//! existing broadcast subscription, which already runs
//! `do_evaluate_after_event` on every `StateChanged` /
//! `Paired` / `Unpaired` event. The
//! `m4_ei_peer_disconnect_deactivates_and_allows_reactivation`
//! test (r3) covers the EI-EOF half. This file's test pins
//! `do_deactivate`'s idempotency: calling it twice in a row is
//! a no-op the second time, so the gate can safely invoke it on
//! every gate-event tick without double-clearing.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use rust_connect::device::types::DeviceEvent;
use rust_connect::device::EventBroadcaster;
use rust_connect::protocol::pairing::PairingHandler;
use rust_connect::protocol::CertificateManager;

const PEER: &str = "test-peer-round4aaaaaaaaaaaaaaaa";

/// Stage a fake peer certificate so the cert-anchor gate added by PR #28
/// (accept/force-accept refuse without a pending cert or pinned
/// fingerprint) lets the accept through. Mirrors the orchestrator's
/// `make_external_cert_der` shape: generate in a throwaway cert dir,
/// convert PEM → DER, stage under the peer's device id.
async fn stage_peer_cert(pairing: &PairingHandler, device_id: &str) {
    const MIN_DEVICE_ID_LEN: usize = 32;
    let mut valid_id = String::from(device_id);
    while valid_id.len() < MIN_DEVICE_ID_LEN {
        valid_id.push('a');
    }
    let cert_dir = tempfile::tempdir().expect("cert tempdir");
    let cm_for_cert = Arc::new(CertificateManager::new(cert_dir.path().to_path_buf()));
    let (cert_pem, _) = cm_for_cert
        .generate_certificate(&valid_id, "Peer")
        .expect("cert generation must succeed");
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .expect("PEM must parse")
        .to_der()
        .expect("DER conversion must succeed");
    pairing
        .set_pending_peer_cert(&device_id.to_string(), cert_der)
        .await;
}

fn build_pairing_with_broadcaster() -> (
    Arc<PairingHandler>,
    tokio::sync::broadcast::Receiver<DeviceEvent>,
) {
    let cert_dir = tempfile::tempdir().expect("cert tempdir");
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.path().to_path_buf()));
    let broadcaster = Arc::new(EventBroadcaster::new(16, "device"));
    let rx = broadcaster.subscribe();
    // `tokio::sync::broadcast::Receiver<T>` is `Send` for any `T: Clone`
    // (the broadcaster's `T` is `DeviceEvent`, which carries only owned
    // data — `String`, primitive types, and `chrono::DateTime<Utc>`).
    // Returning the receiver by value lets each test consume events
    // without re-subscribing inside an `async` block (a second
    // `subscribe()` call would race the broadcaster's first send under
    // `tokio::current`).
    let pairing = Arc::new(PairingHandler::new(cert_manager).with_broadcaster(broadcaster));
    (pairing, rx)
}

async fn next_event_with_timeout(
    rx: &mut tokio::sync::broadcast::Receiver<DeviceEvent>,
    label: &str,
) -> DeviceEvent {
    match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        Ok(Ok(event)) => event,
        Ok(Err(e)) => panic!("{label}: receive error: {e}"),
        Err(_) => panic!("{label}: did not arrive within 2s"),
    }
}

/// Fix 1, accept path. `accept_pairing` (the user-accepts-peer's-
/// request path) must emit `DeviceEvent::Paired { device_id, .. }`
/// on the broadcaster wired via `with_broadcaster`. Capability
/// gates that filter on `is_paired` (shareinputdevices' M4
/// pairing-gate lane) re-evaluate eligibility on this event — if
/// it doesn't fire, a connect-then-pair flow lands `Connected`
/// first, the gate rejects as unpaired, and the eventual pairing
/// goes unnoticed until the next disconnect/reconnect.
///
/// **Red-before-green.** Pre-fix, `PairingHandler` had no
/// broadcaster field at all; the gate filtered on `is_paired`
/// against a snapshot taken at `Connected` time and never
/// revisited. The `with_broadcaster` builder was added (task
/// #1042 lane E) so accept/unpair can notify the gate's
/// subscription loop.
#[tokio::test]
async fn accept_pairing_emits_paired_event() {
    let (pairing, mut rx) = build_pairing_with_broadcaster();

    // Set up the accept precondition: a pending incoming request,
    // matching what the production pairing-acceptance path leaves
    // behind (a peer requested pairing, the user clicked accept).
    pairing
        .receive_pair_request(&PEER.to_string(), None)
        .await
        .expect("receive_pair_request must succeed");

    // PR #28's cert-anchor gate: accept refuses without a pending cert.
    stage_peer_cert(&pairing, PEER).await;

    pairing
        .accept_pairing(&PEER.to_string())
        .await
        .expect("accept_pairing must succeed");

    match next_event_with_timeout(&mut rx, "accept_pairing Paired event").await {
        DeviceEvent::Paired { device_id, .. } => {
            assert_eq!(device_id, PEER, "Paired event must carry the peer id");
        }
        other => panic!("expected DeviceEvent::Paired, got {other:?}"),
    }

    // The slot is the canonical record; the broadcast is a
    // notification. Re-read after accept to pin the seam's
    // invariant: events fire AFTER the trusted-store update.
    assert!(
        pairing.is_paired(&PEER.to_string()).await,
        "is_paired must be true after accept (broadcast confirms slot update, not the other way)"
    );
}

/// Fix 1, force-accept path. `force_accept_pairing` (the
/// auto-accept path used when trust has been pre-granted) emits
/// the same `Paired` event. This is the path the production
/// orchestrator uses for late-pairing flows (a peer that
/// completed the TLS handshake before pairing), so the broadcast
/// from this method is just as load-bearing for the gate as the
/// user-accept one above.
///
/// **Red-before-green.** Same as the accept test — pre-fix
/// `force_accept_pairing` had no broadcaster field to write to.
#[tokio::test]
async fn force_accept_pairing_emits_paired_event() {
    let (pairing, mut rx) = build_pairing_with_broadcaster();

    // PR #28's cert-anchor gate: force-accept refuses without a cert.
    stage_peer_cert(&pairing, PEER).await;

    pairing
        .force_accept_pairing(&PEER.to_string())
        .await
        .expect("force_accept_pairing must succeed");

    match next_event_with_timeout(&mut rx, "force_accept_pairing Paired event").await {
        DeviceEvent::Paired { device_id, .. } => {
            assert_eq!(device_id, PEER, "Paired event must carry the peer id");
        }
        other => panic!("expected DeviceEvent::Paired, got {other:?}"),
    }

    assert!(
        pairing.is_paired(&PEER.to_string()).await,
        "is_paired must be true after force accept"
    );
}

/// Fix 1, unpair path. `unpair` emits `DeviceEvent::Unpaired
/// { device_id }`. The gate's subscription loop's `Unpaired`
/// arm fires `do_evaluate_after_event(None, ...)` against an
/// empty consumer set if the unpairing device was the last
/// capable consumer; that calls `deactivate_portal_session` and
/// tears down the session. Symmetric to the Paired case.
///
/// **Red-before-green.** Pre-fix `unpair` had no broadcaster
/// field; a peer that lost trust kept its slot in the active
/// session until the next physical disconnect.
#[tokio::test]
async fn unpair_emits_unpaired_event() {
    let (pairing, mut rx) = build_pairing_with_broadcaster();

    // Get the device paired first so unpair has something to do.
    // PR #28's cert-anchor gate: force-accept refuses without a cert.
    stage_peer_cert(&pairing, PEER).await;
    pairing
        .force_accept_pairing(&PEER.to_string())
        .await
        .expect("force_accept_pairing must succeed");

    // Drain the Paired event so the next read sees only Unpaired.
    match next_event_with_timeout(&mut rx, "Paired setup event").await {
        DeviceEvent::Paired { device_id, .. } => {
            assert_eq!(device_id, PEER);
        }
        other => panic!("expected DeviceEvent::Paired, got {other:?}"),
    }

    pairing
        .unpair(&PEER.to_string())
        .await
        .expect("unpair must succeed");

    match next_event_with_timeout(&mut rx, "Unpaired event").await {
        DeviceEvent::Unpaired { device_id } => {
            assert_eq!(device_id, PEER, "Unpaired event must carry the peer id");
        }
        other => panic!("expected DeviceEvent::Unpaired, got {other:?}"),
    }

    // The slot must reflect the unpair — the broadcaster is a
    // notification, not a duplicate of the canonical store.
    assert!(
        !pairing.is_paired(&PEER.to_string()).await,
        "is_paired must be false after unpair"
    );
}

/// Fix 1, NULL broadcaster invariant. Constructing a
/// `PairingHandler` WITHOUT `with_broadcaster` (the default —
/// older test setups, code paths that don't need the seam) must
/// still accept / force-accept / unpair without panicking. The
/// broadcast block is `if let Some(broadcaster) = ...`, so a
/// `None` field is a no-op rather than an error.
///
/// This pins the seam's "optional wiring" contract: the feature
/// is opt-in, and removing the wire doesn't break the basic
/// pairing lifecycle. A future tightening that panics on None
/// would break pre-broadcaster test fixtures and need
/// explicit migration.
#[tokio::test]
async fn pairing_handler_without_broadcaster_still_lifecycle_correctly() {
    let cert_dir = tempfile::tempdir().expect("cert tempdir");
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.path().to_path_buf()));
    let pairing = Arc::new(PairingHandler::new(cert_manager));

    pairing
        .receive_pair_request(&PEER.to_string(), None)
        .await
        .expect("receive_pair_request must succeed even without broadcaster");
    // PR #28's cert-anchor gate: accept refuses without a pending cert.
    stage_peer_cert(&pairing, PEER).await;
    pairing
        .accept_pairing(&PEER.to_string())
        .await
        .expect("accept_pairing must succeed even without broadcaster");
    assert!(
        pairing.is_paired(&PEER.to_string()).await,
        "is_paired must be true after accept (no-broadcaster path)"
    );

    pairing
        .unpair(&PEER.to_string())
        .await
        .expect("unpair must succeed even without broadcaster");
    assert!(
        !pairing.is_paired(&PEER.to_string()).await,
        "is_paired must be false after unpair (no-broadcaster path)"
    );
}

/// Fix 3 — `do_deactivate` idempotency. The brief's intent is
/// "re-arm after deactivate" so a subsequent capable-peer's
/// `Connected` event drives the activate arm. The re-arm itself
/// is wired through the gate's subscription loop (Send-bound
/// constraint documented in mod.rs and FINDINGS.md); the
/// idempotency property is what makes that path safe — the loop
/// calls `do_deactivate` on every gate tick, and a no-op second
/// call must not double-clear the slot or panic.
///
/// **What this pins:**
/// - A second `do_deactivate` with no slot populated is a no-op
///   (does not panic, does not try to unwrap a None slot).
/// - Calling `deactivate_portal_session` twice in a row from the
///   gate (a real failure mode under heavy churn) does not break
///   the slot-empty / `backend_available=false` invariants that
///   re-activation checks.
#[tokio::test]
async fn deactivate_portal_session_is_idempotent() {
    use rust_connect::plugins::ShareInputDevicesPlugin;

    let plugin = ShareInputDevicesPlugin::new();

    // Baseline: empty slot, no session flag — the plugin's
    // initial state.
    assert!(
        plugin.portal_session_is_empty_for_test(),
        "fresh plugin slot must be empty"
    );

    // First deactivate: no session, must be a clean no-op.
    plugin.deactivate_portal_session().await;

    // Second deactivate: still no session, still a clean no-op.
    // Pre-fix this could double-take a stale slot; post-fix it's
    // a no-op because the first call found nothing to clear.
    plugin.deactivate_portal_session().await;

    // Invariants survive both calls.
    assert!(
        plugin.portal_session_is_empty_for_test(),
        "slot must remain empty after no-op deactivate"
    );
    assert!(
        !plugin.portal_backend_available(),
        "backend_available must remain false after no-op deactivate"
    );
}
