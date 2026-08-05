use super::*;

fn setup() -> (PairingHandler, tempfile::TempDir) {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager);
    (handler, temp_dir)
}

#[tokio::test]
async fn test_initiate_pairing() {
    let (handler, _temp) = setup();
    assert!(!handler.has_pending_request(&"device-1".to_string()).await);

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    assert!(handler.has_pending_request(&"device-1".to_string()).await);
    assert_eq!(
        handler.pair_state(&"device-1".to_string()).await,
        PairState::Requested
    );
}

#[tokio::test]
async fn test_accept_pairing() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    assert!(handler.is_paired(&"device-1".to_string()).await);
    assert!(!handler.has_pending_request(&"device-1".to_string()).await);
    assert_eq!(
        handler.pair_state(&"device-1".to_string()).await,
        PairState::Paired
    );
}

#[tokio::test]
async fn test_reject_pairing() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    handler
        .reject_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    assert!(!handler.is_paired(&"device-1".to_string()).await);
    assert!(!handler.has_pending_request(&"device-1".to_string()).await);
    assert_eq!(
        handler.pair_state(&"device-1".to_string()).await,
        PairState::NotPaired
    );
}

#[tokio::test]
async fn test_accept_without_pending_returns_error() {
    let (handler, _temp) = setup();
    let result = handler.accept_pairing(&"nonexistent".to_string()).await;
    assert!(result.is_err());
    assert!(!handler.is_paired(&"nonexistent".to_string()).await);
}

#[tokio::test]
async fn test_reject_nonexistent_request() {
    let (handler, _temp) = setup();
    let result = handler.reject_pairing(&"nonexistent".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_duplicate_pairing_initiation() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    let result = handler.initiate_pairing(&"device-1".to_string()).await;
    assert!(result.is_ok());
}

// Pairing-layer self-guard tests. This is defense-in-depth BEHIND three
// existing connection-layer self-guards (landed 2026-07-29):
// `protocol/discovery.rs:185-186` (ignore our own broadcasts),
// `services/connection_orchestrator.rs:207-219` (never dial our own
// identity), `protocol/connection/inbound.rs:93-101` (reject an incoming
// connection claiming our id). Those stop a live remote peer from ever
// reaching pairing code; none of them touch `paired.json`, which only this
// module writes — see the `own_device_id` field doc in mod.rs. Don't read
// the tests below as "nothing was guarded before this" — they cover the
// layer that's actually new.
//
// `with_own_device_id` lets a test configure the id before construction;
// `setup_with_own_id` builds a handler with it set. The three refuse_*
// tests below cover the paths that can originate pairing state: an
// outbound request we initiate, an inbound request a peer sends us, and
// the no-pending-request-required force-accept.
// `test_initiate_pairing_allows_non_own_device_id` is the control — it
// pins the guard to "refuses self", not "refuses everything" (a predicate
// of `own_device_id.is_some()` would pass the three refuse_* tests too).
fn setup_with_own_id(own_id: &str) -> (PairingHandler, tempfile::TempDir) {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager).with_own_device_id(own_id.to_string());
    (handler, temp_dir)
}

#[tokio::test]
async fn test_initiate_pairing_refuses_own_device_id() {
    let (handler, _temp) = setup_with_own_id("self-device-id-aaaaaaaaaaaaaaaa");

    let result = handler
        .initiate_pairing(&"self-device-id-aaaaaaaaaaaaaaaa".to_string())
        .await;

    assert!(result.is_err());
    assert!(
        !handler
            .has_pending_request(&"self-device-id-aaaaaaaaaaaaaaaa".to_string())
            .await
    );
}

#[tokio::test]
async fn test_receive_pair_request_refuses_own_device_id() {
    let (handler, _temp) = setup_with_own_id("self-device-id-aaaaaaaaaaaaaaaa");

    let result = handler
        .receive_pair_request(
            &"self-device-id-aaaaaaaaaaaaaaaa".to_string(),
            Some(Utc::now().timestamp()),
        )
        .await;

    assert!(result.is_err());
    assert_eq!(
        handler
            .pair_state(&"self-device-id-aaaaaaaaaaaaaaaa".to_string())
            .await,
        PairState::NotPaired
    );
}

#[tokio::test]
async fn test_force_accept_pairing_refuses_own_device_id() {
    let (handler, _temp) = setup_with_own_id("self-device-id-aaaaaaaaaaaaaaaa");

    let result = handler
        .force_accept_pairing(&"self-device-id-aaaaaaaaaaaaaaaa".to_string())
        .await;

    assert!(result.is_err());
    assert!(
        !handler
            .is_paired(&"self-device-id-aaaaaaaaaaaaaaaa".to_string())
            .await
    );
}

// Control for the three refuse_* tests above (self-pairing review,
// Important-2): with an own id configured, pairing with a DIFFERENT id —
// the configuration production always runs in — must still succeed. A
// predicate as broad as `own_device_id.is_some()` refuses every peer once
// `set_own_device_id` fires and would still pass all three refuse_* tests;
// only this one catches it.
#[tokio::test]
async fn test_initiate_pairing_allows_non_own_device_id() {
    let (handler, _temp) = setup_with_own_id("self-device-id-aaaaaaaaaaaaaaaa");

    let result = handler
        .initiate_pairing(&"peer-device-id-bbbbbbbbbbbbbbbb".to_string())
        .await;

    assert!(result.is_ok());
    assert_eq!(
        handler
            .pair_state(&"peer-device-id-bbbbbbbbbbbbbbbb".to_string())
            .await,
        PairState::Requested
    );
}

// Self-pairing review, Important-1: `load_from_disk` (called from
// bootstrap.rs, strictly before `Daemon::load_identity` sets the own id)
// is a fourth path into `paired` that the three guards above cannot cover
// — the id isn't known yet when it runs. `set_own_device_id` prunes any
// matching entry once the id becomes known; this reproduces the exact
// on-disk shape a stale/restored/rolled-back `paired.json` would have.
#[tokio::test]
async fn test_set_own_device_id_prunes_self_entry_loaded_from_disk() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let path = temp_dir.path().join("paired.json");

    // Seed paired.json with an entry keyed to what will become our own id.
    // No own id is set on this handler, so the pairing completes exactly
    // like any other — this is what a build predating the guard, a
    // restored backup, or a rollback would have written.
    let seeding_handler = PairingHandler::new(cert_manager.clone()).with_persistence(path.clone());
    seeding_handler
        .initiate_pairing(&"future-self-id-aaaaaaaaaaaaaaaaa".to_string())
        .await
        .expect("Value expected to be present");
    seeding_handler
        .accept_pairing(&"future-self-id-aaaaaaaaaaaaaaaaa".to_string())
        .await
        .expect("Value expected to be present");

    // Fresh handler + load_from_disk stands in for bootstrap.rs's
    // load_persisted_data, which runs before the own id is known.
    let handler = PairingHandler::new(cert_manager).with_persistence(path.clone());
    handler
        .load_from_disk()
        .await
        .expect("Value expected to be present");
    assert!(
        handler
            .is_paired(&"future-self-id-aaaaaaaaaaaaaaaaa".to_string())
            .await
    );

    handler
        .set_own_device_id("future-self-id-aaaaaaaaaaaaaaaaa".to_string())
        .await;

    assert!(
        !handler
            .is_paired(&"future-self-id-aaaaaaaaaaaaaaaaa".to_string())
            .await
    );

    // The prune must persist immediately — otherwise the entry survives on
    // disk until some other save event happens to occur, and an unclean
    // shutdown in between would leave it to be reloaded next boot
    // (stop_services re-persists `paired` on every shutdown regardless).
    let contents = std::fs::read_to_string(&path).expect("Value expected to be present");
    let parsed: HashMap<String, String> =
        serde_json::from_str(&contents).expect("Value expected to be present");
    assert!(!parsed.contains_key("future-self-id-aaaaaaaaaaaaaaaaa"));
}

#[tokio::test]
async fn test_pair_already_paired() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    let result = handler.initiate_pairing(&"device-1".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_unpair() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    handler
        .unpair(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    assert!(!handler.is_paired(&"device-1".to_string()).await);
}

#[tokio::test]
async fn test_unpair_not_paired() {
    let (handler, _temp) = setup();
    let result = handler.unpair(&"nonexistent".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_handle_pair_response_accept() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    handler
        .handle_pair_response(&"device-1".to_string(), true)
        .await
        .expect("Value expected to be present");
    assert!(handler.is_paired(&"device-1".to_string()).await);
}

#[tokio::test]
async fn test_handle_pair_response_reject() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    handler
        .handle_pair_response(&"device-1".to_string(), false)
        .await
        .expect("Value expected to be present");
    assert!(!handler.is_paired(&"device-1".to_string()).await);
}

#[tokio::test]
async fn test_paired_devices() {
    let (handler, _temp) = setup();
    assert!(handler.paired_devices().await.is_empty());

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    handler
        .initiate_pairing(&"device-2".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-2".to_string())
        .await
        .expect("Value expected to be present");

    let mut devices = handler.paired_devices().await;
    devices.sort();
    assert_eq!(devices, vec!["device-1", "device-2"]);
}

#[tokio::test]
async fn test_pending_devices() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    let pending = handler.pending_devices().await;
    assert_eq!(pending, vec!["device-1"]);
}

#[tokio::test]
async fn test_paired_since() {
    let (handler, _temp) = setup();
    assert!(handler
        .paired_since(&"device-1".to_string())
        .await
        .is_none());

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    assert!(handler
        .paired_since(&"device-1".to_string())
        .await
        .is_some());
}

#[tokio::test]
async fn test_cleanup_expired() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager).with_timeout(Duration::milliseconds(50));

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let removed = handler
        .cleanup_expired()
        .await
        .expect("Value expected to be present");
    assert_eq!(removed, 1);
    assert!(!handler.has_pending_request(&"device-1".to_string()).await);
}

#[tokio::test]
async fn test_repair_after_unpair() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .unpair(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    assert_eq!(
        handler.pair_state(&"device-1".to_string()).await,
        PairState::NotPaired
    );

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    assert!(handler.has_pending_request(&"device-1".to_string()).await);
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    assert!(handler.is_paired(&"device-1".to_string()).await);
}

#[tokio::test]
async fn test_accept_expired_request_fails() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager).with_timeout(Duration::milliseconds(50));

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let result = handler.accept_pairing(&"device-1".to_string()).await;
    assert!(result.is_err());
    assert!(!handler.is_paired(&"device-1".to_string()).await);
}

/// Hostile-peer scenario: the peer's pair accept is delayed to JUST INSIDE
/// the request timeout — the pairing must complete in full: paired state,
/// peer cert trusted, paired.json written. (The production requester-side
/// timeout is PAIR_REQUEST_TIMEOUT_SECS = 30s, shrunk here via
/// `with_timeout`; the expiry semantics don't depend on the value.)
#[tokio::test]
async fn test_delayed_pair_response_inside_timeout_completes() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let device_id = "delay-peer-aaaaaaaaaaaaaaaaaaaaaa".to_string();

    // A real peer cert, staged exactly as the packet loop stages it
    // (set_pending_peer_cert) when the peer's accept arrives.
    let (cert_pem, _) = cert_manager
        .generate_certificate(&device_id, "Peer")
        .expect("Value expected to be present");
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");

    let persist_path = temp_dir.path().join("paired.json");
    let handler = PairingHandler::new(cert_manager.clone())
        .with_timeout(Duration::milliseconds(400))
        .with_persistence(persist_path.clone());

    handler
        .initiate_pairing(&device_id)
        .await
        .expect("Value expected to be present");
    handler.set_pending_peer_cert(&device_id, cert_der).await;

    // The accept lands just inside the timeout.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    handler
        .handle_pair_response(&device_id, true)
        .await
        .expect("an accept inside the timeout must complete the pairing");

    assert!(handler.is_paired(&device_id).await);
    assert!(
        cert_manager.has_peer_fingerprint(&device_id),
        "a completed pairing must trust the peer certificate"
    );
    let saved = std::fs::read_to_string(&persist_path)
        .expect("paired.json must be written on a completed pairing");
    assert!(
        saved.contains(&device_id),
        "paired.json must record the device: {saved}"
    );
}

/// Hostile-peer scenario: the peer's pair accept arrives PAST the request
/// timeout — a clean rejection with NO half-paired state: not paired, the
/// pending cert never reaches the trust store, paired.json never written.
#[tokio::test]
async fn test_delayed_pair_response_past_timeout_leaves_no_trace() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let device_id = "delay-peer-aaaaaaaaaaaaaaaaaaaaaa".to_string();

    let (cert_pem, _) = cert_manager
        .generate_certificate(&device_id, "Peer")
        .expect("Value expected to be present");
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");

    let persist_path = temp_dir.path().join("paired.json");
    let handler = PairingHandler::new(cert_manager.clone())
        .with_timeout(Duration::milliseconds(400))
        .with_persistence(persist_path.clone());

    handler
        .initiate_pairing(&device_id)
        .await
        .expect("Value expected to be present");
    handler.set_pending_peer_cert(&device_id, cert_der).await;

    // The accept lands past the timeout.
    tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
    let result = handler.handle_pair_response(&device_id, true).await;
    let err = result.expect_err("an accept past the timeout must be rejected");
    assert!(
        matches!(err, Error::PairingTimeout(_)),
        "unexpected error: {err}"
    );

    assert!(!handler.is_paired(&device_id).await);
    assert_eq!(
        handler.pair_state(&device_id).await,
        PairState::NotPaired,
        "no half-paired state may remain"
    );
    assert!(
        !cert_manager.has_peer_fingerprint(&device_id),
        "a timed-out pairing must NOT trust the peer certificate"
    );
    assert!(
        !persist_path.exists(),
        "paired.json must not be written for a timed-out pairing"
    );
}

/// A pairing that times out must not leave its staged peer cert
/// behind. The cert is staged at TLS handshake (set_pending_peer_cert) for
/// the pairing stretch and is inert once the request expires — the lazy
/// cleanup removed the request but the cert used to linger, so a later
/// accept in a fresh stretch could persist a cert from a DEAD pairing.
#[tokio::test]
async fn test_expired_request_drops_staged_cert() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let device_id = "expire-peer-aaaaaaaaaaaaaaaaaaaaaa".to_string();

    let (cert_pem, _) = cert_manager
        .generate_certificate(&device_id, "Peer")
        .expect("Value expected to be present");
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");

    let handler =
        PairingHandler::new(cert_manager.clone()).with_timeout(Duration::milliseconds(400));

    handler
        .initiate_pairing(&device_id)
        .await
        .expect("Value expected to be present");
    handler.set_pending_peer_cert(&device_id, cert_der).await;

    // The accept lands past the timeout: rejected, request cleared — and
    // the staged cert must be cleared with it.
    tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
    let err = handler
        .handle_pair_response(&device_id, true)
        .await
        .expect_err("an accept past the timeout must be rejected");
    assert!(
        matches!(err, Error::PairingTimeout(_)),
        "unexpected error: {err}"
    );
    assert_eq!(handler.pair_state(&device_id).await, PairState::NotPaired);

    // A fresh pairing stretch with no re-staged cert (the unit-level
    // equivalent of a peer whose accept arrives without a re-staged cert):
    // accept must find nothing to persist — the dead stretch's cert is gone.
    handler
        .receive_pair_request(&device_id, Some(Utc::now().timestamp()))
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&device_id)
        .await
        .expect("a fresh, unexpired request must accept");
    assert!(
        !cert_manager.has_peer_fingerprint(&device_id),
        "the timed-out pairing's staged cert must not leak into a later accept"
    );
}

/// Follow-up to the reap invariant above: `cleanup_expired` is the OTHER
/// lazy-expiry path — invoked from `initiate_pairing` and
/// `receive_pair_request` — and must drop staged certs for the requests it
/// reaps. Before the follow-up fix it swept the request maps only, so a
/// cert from a dead stretch still lingered whenever ANY device's pairing
/// activity triggered the sweep.
#[tokio::test]
async fn test_cleanup_expired_drops_staged_cert() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let device_id = "sweep-peer-bbbbbbbbbbbbbbbbbbbbbb".to_string();

    let (cert_pem, _) = cert_manager
        .generate_certificate(&device_id, "Peer")
        .expect("Value expected to be present");
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");

    let handler =
        PairingHandler::new(cert_manager.clone()).with_timeout(Duration::milliseconds(400));

    handler
        .initiate_pairing(&device_id)
        .await
        .expect("Value expected to be present");
    handler.set_pending_peer_cert(&device_id, cert_der).await;

    // The request expires; the sweep (not an accept attempt) reaps it — and
    // must reap the staged cert with it.
    tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
    let removed = handler
        .cleanup_expired()
        .await
        .expect("Value expected to be present");
    assert_eq!(removed, 1, "the expired request must be swept");

    // A fresh pairing stretch with no re-staged cert: accept must find
    // nothing to persist — the swept stretch's cert is gone.
    handler
        .receive_pair_request(&device_id, Some(Utc::now().timestamp()))
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&device_id)
        .await
        .expect("a fresh, unexpired request must accept");
    assert!(
        !cert_manager.has_peer_fingerprint(&device_id),
        "a swept pairing's staged cert must not leak into a later accept"
    );
}

/// The flip side of the reap invariant: the sweep must not wipe the cert
/// staged for the CURRENT packet. The real inbound flow's dead-stretch
/// scenario — an expired request for this device lingers, then a fresh
/// pair request arrives carrying its cert: the reap of the dead stretch
/// must leave the fresh cert in place and the accept must persist it.
/// connection_loop stages via receive_pair_request_with_cert, which
/// stages after cleanup_expired; staging before the sweep would lose the
/// cert to this device's own reap — a silently cert-less pairing.
#[tokio::test]
async fn test_sweep_preserves_freshly_staged_cert() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let device_id = "sweep-peer-cccccccccccccccccccccc".to_string();

    let (cert_pem, _) = cert_manager
        .generate_certificate(&device_id, "Peer")
        .expect("Value expected to be present");
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");

    let handler = PairingHandler::new(cert_manager.clone())
        .with_timeout(Duration::milliseconds(400))
        .with_accept_timeout(Duration::milliseconds(400));

    // Dead stretch: an incoming request with no staged cert, left to expire.
    handler
        .receive_pair_request(&device_id, Some(Utc::now().timestamp()))
        .await
        .expect("Value expected to be present");
    tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;

    // Fresh stretch: the request carries its cert (the connection-loop
    // order). The sweep reaps the dead stretch inside this call.
    handler
        .receive_pair_request_with_cert(&device_id, Some(Utc::now().timestamp()), Some(cert_der))
        .await
        .expect("Value expected to be present");

    // The accept must persist the fresh cert — if the sweep had wiped it,
    // this pairs the device with NO certificate (verify-before-write
    // bypassed).
    handler
        .accept_pairing(&device_id)
        .await
        .expect("the fresh, unexpired request must accept");
    assert!(
        cert_manager.has_peer_fingerprint(&device_id),
        "the cert staged with the fresh request must survive the dead stretch's reap"
    );
}

/// A cert must be staged only
/// once its request is known LIVE. A request rejected by the max-pending
/// cap takes no slot and has no expiry path — if its cert were staged
/// anyway it would orphan in pending_certs, and a later certless stretch
/// for the same device would persist the orphan at accept.
#[tokio::test]
async fn test_rejected_request_stages_no_cert() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let device_a = "orphan-a-aaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let device_b = "orphan-b-bbbbbbbbbbbbbbbbbbbbbbb".to_string();

    let (cert_pem, _) = cert_manager
        .generate_certificate(&device_b, "PeerB")
        .expect("Value expected to be present");
    let cert_der_b = openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");

    let handler = PairingHandler::new(cert_manager.clone()).with_max_pending(1);

    // A fills the single pending slot.
    handler
        .receive_pair_request(&device_a, Some(Utc::now().timestamp()))
        .await
        .expect("Value expected to be present");

    // B's request — carrying a cert — is rejected by the cap. The cert must
    // NOT be staged: the request it belonged to never existed.
    let err = handler
        .receive_pair_request_with_cert(&device_b, Some(Utc::now().timestamp()), Some(cert_der_b))
        .await
        .expect_err("the over-cap request must be rejected");
    assert!(
        matches!(err, Error::PairingRejected(_)),
        "unexpected error: {err}"
    );

    // A pairs and frees the slot; B retries certless (as after a link
    // reconnect that staged nothing). Accept must find nothing to persist.
    handler
        .accept_pairing(&device_a)
        .await
        .expect("Value expected to be present");
    handler
        .receive_pair_request(&device_b, Some(Utc::now().timestamp()))
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&device_b)
        .await
        .expect("B's fresh request must accept");
    assert!(
        !cert_manager.has_peer_fingerprint(&device_b),
        "a rejected request's cert must not orphan into a later accept"
    );
}

/// pair_state must not report Requested/RequestedByPeer for entries past
/// their timeout: the maps are cleaned lazily, and a stale outgoing request
/// mis-classified the test phone's fresh pair request as the accept of our
/// dead one (live failure, 2026-07-30).
#[tokio::test]
async fn test_pair_state_ignores_expired_requests() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager)
        .with_timeout(Duration::milliseconds(50))
        .with_accept_timeout(Duration::milliseconds(50));

    let out_dev = "device-out".to_string();
    handler
        .initiate_pairing(&out_dev)
        .await
        .expect("Value expected to be present");
    assert_eq!(handler.pair_state(&out_dev).await, PairState::Requested);

    let in_dev = "device-in".to_string();
    handler
        .receive_pair_request(&in_dev, Some(Utc::now().timestamp()))
        .await
        .expect("Value expected to be present");
    assert_eq!(
        handler.pair_state(&in_dev).await,
        PairState::RequestedByPeer
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    assert_eq!(handler.pair_state(&out_dev).await, PairState::NotPaired);
    assert_eq!(handler.pair_state(&in_dev).await, PairState::NotPaired);
}

#[tokio::test]
async fn test_cleanup_preserves_non_expired() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager).with_timeout(Duration::seconds(60));

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .initiate_pairing(&"device-2".to_string())
        .await
        .expect("Value expected to be present");

    let removed = handler
        .cleanup_expired()
        .await
        .expect("Value expected to be present");
    assert_eq!(removed, 0);
    assert!(handler.has_pending_request(&"device-1".to_string()).await);
    assert!(handler.has_pending_request(&"device-2".to_string()).await);
}

#[tokio::test]
async fn test_multiple_pending_devices() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"a".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .initiate_pairing(&"b".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .initiate_pairing(&"c".to_string())
        .await
        .expect("Value expected to be present");

    let mut pending = handler.pending_devices().await;
    pending.sort();
    assert_eq!(pending, vec!["a", "b", "c"]);

    handler
        .accept_pairing(&"b".to_string())
        .await
        .expect("Value expected to be present");

    assert!(handler.is_paired(&"b".to_string()).await);
    assert!(handler.has_pending_request(&"a".to_string()).await);
    assert!(handler.has_pending_request(&"c".to_string()).await);
    assert_eq!(handler.paired_devices().await, vec!["b"]);
}

#[tokio::test]
async fn test_pair_state_not_paired() {
    let (handler, _temp) = setup();
    assert_eq!(
        handler.pair_state(&"ghost".to_string()).await,
        PairState::NotPaired
    );
}

#[tokio::test]
async fn test_paired_since_none_when_not_paired() {
    let (handler, _temp) = setup();
    assert!(handler.paired_since(&"ghost".to_string()).await.is_none());
}

#[tokio::test]
async fn test_is_paired_false() {
    let (handler, _temp) = setup();
    assert!(!handler.is_paired(&"nobody".to_string()).await);
}

#[tokio::test]
async fn test_rate_limit_rejects_when_max_pending() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager)
        .with_max_pending(3)
        .with_timeout(Duration::seconds(60));

    handler
        .initiate_pairing(&"a".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .initiate_pairing(&"b".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .initiate_pairing(&"c".to_string())
        .await
        .expect("Value expected to be present");

    let result = handler.initiate_pairing(&"d".to_string()).await;
    assert!(result.is_err());
    assert!(result
        .expect_err("pairing should be rejected")
        .to_string()
        .contains("Too many pending"));
}

#[tokio::test]
async fn test_save_and_load_paired_devices() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let path = temp_dir.path().join("paired.json");

    let handler = PairingHandler::new(cert_manager.clone())
        .with_persistence(path.clone())
        .with_timeout(Duration::seconds(60));

    handler
        .initiate_pairing(&"dev-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"dev-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .initiate_pairing(&"dev-2".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"dev-2".to_string())
        .await
        .expect("Value expected to be present");

    assert!(path.exists());
    let contents =
        std::fs::read_to_string(&path).expect("Serialization of known types cannot fail");
    let parsed: HashMap<String, String> =
        serde_json::from_str(&contents).expect("Value expected to be present");
    assert!(parsed.contains_key("dev-1"));
    assert!(parsed.contains_key("dev-2"));

    let handler2 = PairingHandler::new(cert_manager)
        .with_persistence(path)
        .with_timeout(Duration::seconds(60));
    handler2
        .load_from_disk()
        .await
        .expect("Value expected to be present");

    assert!(handler2.is_paired(&"dev-1".to_string()).await);
    assert!(handler2.is_paired(&"dev-2".to_string()).await);
    assert!(handler2.paired_since(&"dev-1".to_string()).await.is_some());

    let mut devices = handler2.paired_devices().await;
    devices.sort();
    assert_eq!(devices, vec!["dev-1", "dev-2"]);
}

#[tokio::test]
async fn test_load_from_missing_file_starts_empty() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let path = temp_dir.path().join("nonexistent.json");

    let handler = PairingHandler::new(cert_manager)
        .with_persistence(path)
        .with_timeout(Duration::seconds(60));

    handler
        .load_from_disk()
        .await
        .expect("Value expected to be present");
    assert!(handler.paired_devices().await.is_empty());
}

#[tokio::test]
async fn test_save_load_preserves_paired_state() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let path = temp_dir.path().join("paired.json");

    let handler1 = PairingHandler::new(cert_manager.clone())
        .with_persistence(path.clone())
        .with_timeout(Duration::seconds(60));

    handler1
        .initiate_pairing(&"phone".to_string())
        .await
        .expect("Value expected to be present");
    handler1
        .accept_pairing(&"phone".to_string())
        .await
        .expect("Value expected to be present");

    let handler2 = PairingHandler::new(cert_manager.clone())
        .with_persistence(path.clone())
        .with_timeout(Duration::seconds(60));
    handler2
        .load_from_disk()
        .await
        .expect("Value expected to be present");

    assert_eq!(
        handler2.pair_state(&"phone".to_string()).await,
        PairState::Paired
    );

    let result = handler2.initiate_pairing(&"phone".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_unpair_removes_peer_cert_and_fingerprint() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    cert_manager
        .ensure_certificate("peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa", "Peer")
        .expect("Value expected to be present");
    let (cert_pem, _key_pem) = cert_manager
        .generate_certificate("peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa", "Peer")
        .expect("Value expected to be present");
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");
    cert_manager
        .store_peer_certificate("peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa", &cert_der)
        .expect("Value expected to be present");

    assert!(cert_manager.has_peer_certificate("peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa"));
    assert!(cert_manager.has_peer_fingerprint("peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa"));

    let handler = PairingHandler::new(cert_manager.clone());
    handler
        .initiate_pairing(&"peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .unpair(&"peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        .await
        .expect("Value expected to be present");

    assert!(!cert_manager.has_peer_certificate("peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa"));
    assert!(!cert_manager.has_peer_fingerprint("peer-1aaaaaaaaaaaaaaaaaaaaaaaaaa"));
}

#[tokio::test]
async fn test_list_known_peers() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let (cert_pem_a, _) = cert_manager
        .generate_certificate("aliceaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Alice")
        .expect("Value expected to be present");
    let cert_der_a = openssl::x509::X509::from_pem(&cert_pem_a)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");
    cert_manager
        .store_peer_certificate("aliceaaaaaaaaaaaaaaaaaaaaaaaaaaa", &cert_der_a)
        .expect("Value expected to be present");

    let (cert_pem_b, _) = cert_manager
        .generate_certificate("bobaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Bob")
        .expect("Value expected to be present");
    let cert_der_b = openssl::x509::X509::from_pem(&cert_pem_b)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");
    cert_manager
        .store_peer_certificate("bobaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &cert_der_b)
        .expect("Value expected to be present");

    let mut peers = cert_manager
        .list_known_peers()
        .expect("Value expected to be present");
    peers.sort();
    assert_eq!(
        peers,
        vec![
            "aliceaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bobaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ]
    );
}

#[tokio::test]
async fn test_list_known_peers_empty() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let peers = cert_manager
        .list_known_peers()
        .expect("Value expected to be present");
    assert!(peers.is_empty());
}

#[tokio::test]
async fn test_no_persistence_path_skips_save() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let handler = PairingHandler::new(cert_manager);
    handler
        .initiate_pairing(&"dev".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"dev".to_string())
        .await
        .expect("Value expected to be present");

    assert!(!temp_dir.path().join("paired.json").exists());
}

#[tokio::test]
async fn test_receive_pair_request() {
    let (handler, _temp) = setup();
    handler
        .receive_pair_request(&"device-1".to_string(), Some(1_700_000_000))
        .await
        .expect("Value expected to be present");

    assert!(handler.has_incoming_request(&"device-1".to_string()).await);
    assert_eq!(
        handler.pair_state(&"device-1".to_string()).await,
        PairState::RequestedByPeer
    );
}

#[tokio::test]
async fn test_auto_accept_when_both_initiate() {
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .receive_pair_request(&"device-1".to_string(), Some(1_700_000_000))
        .await
        .expect("Value expected to be present");

    assert!(handler.is_paired(&"device-1".to_string()).await);
    assert!(!handler.has_pending_request(&"device-1".to_string()).await);
}

#[tokio::test]
async fn test_receive_pair_request_while_paired() {
    // Android semantics (PairingHandler.kt:60-68): a pair request from an
    // already-paired device unpairs BOTH sides and starts over as a fresh
    // incoming request — never a silent re-confirm.
    let (handler, _temp) = setup();
    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    let result = handler
        .receive_pair_request(&"device-1".to_string(), Some(1_700_000_000))
        .await;
    assert!(result.is_ok());
    assert!(
        !handler.is_paired(&"device-1".to_string()).await,
        "pair request while paired must unpair (Android semantics)"
    );
    assert!(
        handler.has_incoming_request(&"device-1".to_string()).await,
        "the request must restart as a fresh incoming pairing"
    );
}

#[tokio::test]
async fn test_incoming_requests() {
    let (handler, _temp) = setup();
    handler
        .receive_pair_request(&"a".to_string(), Some(1_700_000_000))
        .await
        .expect("Value expected to be present");
    handler
        .receive_pair_request(&"b".to_string(), Some(1_700_000_000))
        .await
        .expect("Value expected to be present");

    let mut incoming = handler.incoming_requests().await;
    incoming.sort();
    assert_eq!(incoming, vec!["a", "b"]);
}

#[tokio::test]
async fn test_accept_pairing_with_incoming_only() {
    let (handler, _temp) = setup();
    handler
        .receive_pair_request(&"device-1".to_string(), Some(1_700_000_000))
        .await
        .expect("Value expected to be present");

    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    assert!(handler.is_paired(&"device-1".to_string()).await);
    assert!(!handler.has_incoming_request(&"device-1".to_string()).await);
}

#[tokio::test]
async fn test_receive_pair_request_rate_limited() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager)
        .with_max_pending(3)
        .with_timeout(Duration::seconds(60));

    handler
        .initiate_pairing(&"a".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .initiate_pairing(&"b".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .initiate_pairing(&"c".to_string())
        .await
        .expect("Value expected to be present");

    let result = handler
        .receive_pair_request(&"d".to_string(), Some(1_700_000_000))
        .await;
    assert!(result.is_err());
    assert!(result
        .expect_err("pairing should be rejected")
        .to_string()
        .contains("Too many pending"));
}

#[tokio::test]
async fn test_receive_pair_request_duplicate_ignored() {
    // Android PairingHandler.kt:53-58: a second pairing request while one is
    // pending is IGNORED — the first request (and its SAS timestamp) stands,
    // never overwritten.
    let (handler, _temp) = setup();
    let peer_id = "dup-device-aaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    handler
        .cert_manager
        .ensure_own_certificate("our-device-aaaaaaaaaaaaaaaaaaaaaaaa", "Us")
        .expect("Value expected to be present");
    let cm_for_cert = Arc::new(CertificateManager::new(
        tempfile::TempDir::new()
            .expect("Value expected to be present")
            .path()
            .to_path_buf(),
    ));
    let cert_der = make_cert_der(&cm_for_cert, &peer_id, "Peer");

    let first_ts = 1_700_000_000i64;
    let second_ts = 1_700_000_999i64;
    handler
        .receive_pair_request(&peer_id, Some(first_ts))
        .await
        .expect("Value expected to be present");
    handler
        .set_pending_peer_cert(&peer_id, cert_der.clone())
        .await;
    handler
        .receive_pair_request(&peer_id, Some(second_ts))
        .await
        .expect("Value expected to be present");

    assert_eq!(
        handler.pair_state(&peer_id).await,
        PairState::RequestedByPeer
    );
    assert!(handler.has_incoming_request(&peer_id).await);

    // The SAS must be computed over the FIRST request's timestamp — the
    // duplicate must not have overwritten it.
    let key = handler
        .get_verification_key(&peer_id)
        .await
        .expect("Value expected to be present")
        .expect("SAS must exist while pairing is pending");
    let (our_pem, _) = handler
        .cert_manager
        .load_own_certificate()
        .expect("Value expected to be present");
    let our_der = openssl::x509::X509::from_pem(&our_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");
    let our_pub =
        CertificateManager::extract_pubkey_der(&our_der).expect("Value expected to be present");
    let peer_pub =
        CertificateManager::extract_pubkey_der(&cert_der).expect("Value expected to be present");
    assert_eq!(
        key,
        CertificateManager::compute_verification_key(&our_pub, &peer_pub, first_ts),
        "duplicate request must not overwrite the first request's timestamp"
    );
    assert_ne!(
        key,
        CertificateManager::compute_verification_key(&our_pub, &peer_pub, second_ts),
        "the ignored duplicate's timestamp must not reach the SAS"
    );

    handler
        .accept_pairing(&peer_id)
        .await
        .expect("Value expected to be present");
    assert!(handler.is_paired(&peer_id).await);
}

#[tokio::test]
async fn test_receive_pair_request_without_timestamp_accepted_as_v7() {
    // Protocol v7 peers send no timestamp — Android gates the requirement on
    // protocolVersion >= 8 (PairingHandler.kt:71-83), and the loop enforces
    // it there. At handler level a timestamp-less request is a valid v7
    // request: it is stored (timestamp sentinel 0, no SAS surfaces).
    let (handler, _temp) = setup();

    handler
        .receive_pair_request(&"device-1".to_string(), None)
        .await
        .expect("v7 timestamp-less request must be accepted");
    assert!(handler.has_incoming_request(&"device-1".to_string()).await);
    assert_eq!(
        handler.pair_state(&"device-1".to_string()).await,
        PairState::RequestedByPeer
    );
}

#[tokio::test]
async fn test_default_timeouts_match_android() {
    // Android: 30s waiting for the peer's accept (requester,
    // PairingHandler.kt:151), 25s waiting for the local user's response
    // (accepter, PairingHandler.kt:88).
    assert_eq!(PAIR_REQUEST_TIMEOUT_SECS, 30);
    assert_eq!(PAIR_ACCEPT_TIMEOUT_SECS, 25);
}

#[tokio::test]
async fn test_incoming_request_uses_accept_timeout() {
    // The accepter-side timeout applies to incoming requests; the
    // requester-side timeout to outgoing ones.
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager)
        .with_timeout(Duration::seconds(60))
        .with_accept_timeout(Duration::milliseconds(50));

    handler
        .initiate_pairing(&"out".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .receive_pair_request(&"in".to_string(), Some(1_700_000_000))
        .await
        .expect("Value expected to be present");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let removed = handler
        .cleanup_expired()
        .await
        .expect("Value expected to be present");
    assert_eq!(removed, 1, "only the incoming request must expire");
    assert!(handler.has_pending_request(&"out".to_string()).await);
    assert!(!handler.has_pending_request(&"in".to_string()).await);
}

#[tokio::test]
async fn test_pending_devices_includes_both_maps() {
    let (handler, _temp) = setup();

    handler
        .initiate_pairing(&"outgoing-dev".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .receive_pair_request(&"incoming-dev".to_string(), Some(1_700_000_000))
        .await
        .expect("Value expected to be present");

    let mut pending = handler.pending_devices().await;
    pending.sort();
    assert_eq!(pending, vec!["incoming-dev", "outgoing-dev"]);
}

#[tokio::test]
async fn test_load_from_disk_corrupt_json() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let path = temp_dir.path().join("paired.json");

    std::fs::write(&path, "this is not json {{{").expect("Value expected to be present");

    let handler = PairingHandler::new(cert_manager)
        .with_persistence(path)
        .with_timeout(Duration::seconds(60));
    let result = handler.load_from_disk().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_concurrent_initiate_pairing_same_device() {
    let (handler, _temp) = setup();
    let handler = Arc::new(handler);
    let device_id: DeviceId = "device-1".to_string();

    let h1 = handler.clone();
    let h2 = handler.clone();
    let d1 = device_id.clone();
    let d2 = device_id.clone();

    let t1: tokio::task::JoinHandle<std::result::Result<(), crate::utils::errors::Error>> =
        tokio::spawn(async move { h1.initiate_pairing(&d1).await.map(|_| ()) });
    let t2: tokio::task::JoinHandle<std::result::Result<(), crate::utils::errors::Error>> =
        tokio::spawn(async move { h2.initiate_pairing(&d2).await.map(|_| ()) });

    let (r1, r2) = tokio::join!(t1, t2);
    let r1 = r1.expect("Value expected to be present");
    let r2 = r2.expect("Value expected to be present");
    assert!(
        r1.is_ok() || r2.is_ok(),
        "At least one concurrent initiate should succeed"
    );
    assert!(
        handler.has_pending_request(&device_id).await || handler.is_paired(&device_id).await,
        "Device should be in pending or paired state"
    );
}

#[tokio::test]
async fn test_concurrent_accept_and_initiate() {
    let (handler, _temp) = setup();
    let handler = Arc::new(handler);
    let device_id: DeviceId = "device-1".to_string();

    handler
        .initiate_pairing(&device_id)
        .await
        .expect("Value expected to be present");

    let h1 = handler.clone();
    let h2 = handler.clone();
    let d1 = device_id.clone();
    let d2 = device_id.clone();

    let t1: tokio::task::JoinHandle<std::result::Result<(), crate::utils::errors::Error>> =
        tokio::spawn(async move { h1.accept_pairing(&d1).await });
    let t2: tokio::task::JoinHandle<std::result::Result<(), crate::utils::errors::Error>> =
        tokio::spawn(async move { h2.initiate_pairing(&d2).await.map(|_| ()) });

    let (r1, r2) = tokio::join!(t1, t2);
    assert!(r1.expect("Value expected to be present").is_ok());
    assert!(r2.expect("Value expected to be present").is_err());
    assert!(handler.is_paired(&device_id).await);
}

#[tokio::test]
async fn test_get_verification_key_returns_none_when_no_pending() {
    let (handler, _temp) = setup();
    let key = handler.get_verification_key(&"device-1".to_string()).await;
    assert!(key.is_ok());
    assert!(key.expect("Value expected to be present").is_none());
}

/// A stale entry must surface NO key: an accept fired against a stale SAS
/// during test-phone live validation (2026-07-30) hit the expired request
/// instead of the phone's fresh one.
#[tokio::test]
async fn test_get_verification_key_returns_none_when_expired() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager)
        .with_timeout(Duration::milliseconds(50))
        .with_accept_timeout(Duration::milliseconds(50));

    let out_dev = "device-out".to_string();
    handler
        .initiate_pairing(&out_dev)
        .await
        .expect("Value expected to be present");
    let in_dev = "device-in".to_string();
    handler
        .receive_pair_request(&in_dev, Some(Utc::now().timestamp()))
        .await
        .expect("Value expected to be present");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    assert_eq!(handler.pair_state(&out_dev).await, PairState::NotPaired);
    assert!(handler
        .get_verification_key(&out_dev)
        .await
        .expect("Value expected to be present")
        .is_none());
    assert!(handler
        .get_verification_key(&in_dev)
        .await
        .expect("Value expected to be present")
        .is_none());

    // The live-lookup helpers must not see expired entries either — a stale
    // incoming entry sent an unrequested pair=true onto the wire during
    // test-phone live validation (pair_device's accept branch fired on it,
    // 2026-07-30).
    assert!(!handler.has_incoming_request(&in_dev).await);
    assert!(!handler.has_pending_request(&in_dev).await);
    assert!(!handler.has_pending_request(&out_dev).await);
}

#[tokio::test]
async fn test_max_pending_boundary_at_one() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let handler = PairingHandler::new(cert_manager)
        .with_max_pending(1)
        .with_timeout(Duration::seconds(60));

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    let result = handler.initiate_pairing(&"device-2".to_string()).await;
    assert!(result.is_err());
    assert!(result
        .expect_err("pairing should be rejected")
        .to_string()
        .contains("Too many pending"));
}

#[tokio::test]
async fn test_persistence_write_and_reload() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");
    let path = temp_dir.path().join("paired.json");

    let handler = PairingHandler::new(cert_manager.clone())
        .with_persistence(path.clone())
        .with_timeout(Duration::seconds(60));

    handler
        .initiate_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");
    assert!(handler.is_paired(&"device-1".to_string()).await);

    let handler2 = PairingHandler::new(cert_manager)
        .with_persistence(path)
        .with_timeout(Duration::seconds(60));
    handler2
        .load_from_disk()
        .await
        .expect("Value expected to be present");
    assert!(handler2.is_paired(&"device-1".to_string()).await);
}

#[tokio::test]
async fn test_load_from_disk_corrupt_json_fails() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let path = temp_dir.path().join("paired.json");
    std::fs::write(&path, "not valid json{{{").expect("Value expected to be present");

    let handler = PairingHandler::new(cert_manager).with_persistence(path);

    let result = handler.load_from_disk().await;
    assert!(
        result.is_err(),
        "Should fail when persistence file contains corrupt JSON"
    );
}

#[tokio::test]
async fn test_save_to_disk_unwritable_parent_fails() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager.init().expect("Value expected to be present");

    let path = std::path::PathBuf::from("/proc/1/paired.json");
    let handler = PairingHandler::new(cert_manager).with_persistence(path);
    handler
        .receive_pair_request(&"device-1".to_string(), Some(1_700_000_000))
        .await
        .expect("Value expected to be present");
    handler
        .accept_pairing(&"device-1".to_string())
        .await
        .expect("Value expected to be present");

    let result = handler.save_to_disk().await;
    assert!(
        result.is_err(),
        "Should fail when parent directory cannot be created"
    );
}

// ---- P4: verify-before-write + SAS conformance ----

/// Generate a cert for `id` and return its DER.
fn make_cert_der(cm: &CertificateManager, id: &str, name: &str) -> Vec<u8> {
    let (cert_pem, _) = cm
        .generate_certificate(id, name)
        .expect("Value expected to be present");
    openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present")
}

#[tokio::test]
async fn test_peer_cert_not_stored_until_pairing_confirmed() {
    let (handler, _temp) = setup();
    let peer_id = "peer-verify-before-write-aaaaaaaa".to_string();

    let cm_for_cert = Arc::new(CertificateManager::new(
        tempfile::TempDir::new()
            .expect("Value expected to be present")
            .path()
            .to_path_buf(),
    ));
    let cert_der = make_cert_der(&cm_for_cert, &peer_id, "Peer");

    handler
        .receive_pair_request(&peer_id, Some(1_700_000_000))
        .await
        .expect("Value expected to be present");
    handler.set_pending_peer_cert(&peer_id, cert_der).await;

    assert!(
        !handler.cert_manager.has_peer_certificate(&peer_id),
        "cert must NOT hit disk before the pairing is confirmed"
    );
    assert!(!handler.cert_manager.has_peer_fingerprint(&peer_id));

    handler
        .accept_pairing(&peer_id)
        .await
        .expect("Value expected to be present");

    assert!(
        handler.cert_manager.has_peer_certificate(&peer_id),
        "cert must be persisted exactly when the pairing is confirmed"
    );
    assert!(handler.cert_manager.has_peer_fingerprint(&peer_id));
}

#[tokio::test]
async fn test_pending_cert_cleared_on_reject() {
    let (handler, _temp) = setup();
    let peer_id = "peer-reject-clear-aaaaaaaaaaaaaaa".to_string();

    let cm_for_cert = Arc::new(CertificateManager::new(
        tempfile::TempDir::new()
            .expect("Value expected to be present")
            .path()
            .to_path_buf(),
    ));
    let cert_der = make_cert_der(&cm_for_cert, &peer_id, "Peer");

    handler
        .receive_pair_request(&peer_id, Some(1_700_000_000))
        .await
        .expect("Value expected to be present");
    handler.set_pending_peer_cert(&peer_id, cert_der).await;
    handler
        .reject_pairing(&peer_id)
        .await
        .expect("Value expected to be present");

    assert!(!handler.cert_manager.has_peer_certificate(&peer_id));
    assert!(!handler.cert_manager.has_peer_fingerprint(&peer_id));
}

#[tokio::test]
async fn test_mutual_auto_accept_persists_cert() {
    let (handler, _temp) = setup();
    let peer_id = "peer-mutual-aaaaaaaaaaaaaaaaaaaaa".to_string();

    let cm_for_cert = Arc::new(CertificateManager::new(
        tempfile::TempDir::new()
            .expect("Value expected to be present")
            .path()
            .to_path_buf(),
    ));
    let cert_der = make_cert_der(&cm_for_cert, &peer_id, "Peer");

    handler
        .initiate_pairing(&peer_id)
        .await
        .expect("Value expected to be present");
    handler.set_pending_peer_cert(&peer_id, cert_der).await;
    // Peer also initiated: auto-accept path.
    handler
        .receive_pair_request(&peer_id, Some(1_700_000_000))
        .await
        .expect("Value expected to be present");

    assert!(handler.is_paired(&peer_id).await);
    assert!(handler.cert_manager.has_peer_certificate(&peer_id));
}

#[tokio::test]
async fn test_verification_key_matches_android_algorithm_and_timestamp() {
    let (handler, _temp) = setup();
    let peer_id = "peer-sas-aaaaaaaaaaaaaaaaaaaaaaaa".to_string();

    // Own identity is required (the SAS includes our pubkey).
    handler
        .cert_manager
        .ensure_own_certificate("our-device-aaaaaaaaaaaaaaaaaaaaaaaa", "Us")
        .expect("Value expected to be present");

    let cm_for_cert = Arc::new(CertificateManager::new(
        tempfile::TempDir::new()
            .expect("Value expected to be present")
            .path()
            .to_path_buf(),
    ));
    let cert_der = make_cert_der(&cm_for_cert, &peer_id, "Peer");

    let pair_ts = 1700000000i64;
    handler
        .receive_pair_request(&peer_id, Some(pair_ts))
        .await
        .expect("Value expected to be present");
    handler
        .set_pending_peer_cert(&peer_id, cert_der.clone())
        .await;

    let key = handler
        .get_verification_key(&peer_id)
        .await
        .expect("Value expected to be present")
        .expect("SAS must exist while pairing is pending");

    // Independently recompute with the same algorithm.
    let (our_pem, _) = handler
        .cert_manager
        .load_own_certificate()
        .expect("Value expected to be present");
    let our_der = openssl::x509::X509::from_pem(&our_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");
    let our_pub =
        CertificateManager::extract_pubkey_der(&our_der).expect("Value expected to be present");
    let peer_pub =
        CertificateManager::extract_pubkey_der(&cert_der).expect("Value expected to be present");
    let expected = CertificateManager::compute_verification_key(&our_pub, &peer_pub, pair_ts);

    assert_eq!(key, expected);
    assert!(
        key.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()),
        "SAS must be 8 uppercase hex chars, got {}",
        key
    );

    // A different pair-packet timestamp MUST yield a different key (this is
    // what makes the phone's and our display match: both use the request's
    // timestamp, not "now").
    let other = CertificateManager::compute_verification_key(&our_pub, &peer_pub, pair_ts + 1);
    assert_ne!(key, other);
}

/// The SAS must be computable the moment an incoming request (carrying its
/// cert) is staged — the daemon logs it then, and API surfaces read it for
/// display before the accept.
#[tokio::test]
async fn test_verification_key_available_while_incoming_request_pending() {
    let temp_dir = tempfile::TempDir::new().expect("Value expected to be present");
    let cert_manager = Arc::new(CertificateManager::new(temp_dir.path().to_path_buf()));
    cert_manager
        .ensure_own_certificate("test-daemon-aaaaaaaaaaaaaaaaaaaaaa", "Test Daemon")
        .expect("Value expected to be present");
    let device_id = "sas-visible-aaaaaaaaaaaaaaaaaaaaaa".to_string();

    let (cert_pem, _) = cert_manager
        .generate_certificate(&device_id, "Peer")
        .expect("Value expected to be present");
    let cert_der = openssl::x509::X509::from_pem(&cert_pem)
        .expect("Value expected to be present")
        .to_der()
        .expect("Value expected to be present");

    let handler = PairingHandler::new(cert_manager.clone());
    handler
        .receive_pair_request_with_cert(&device_id, Some(Utc::now().timestamp()), Some(cert_der))
        .await
        .expect("Value expected to be present");

    let key = handler
        .get_verification_key(&device_id)
        .await
        .expect("Value expected to be present");
    assert!(
        key.is_some(),
        "SAS must be available while the incoming request is pending"
    );
}
