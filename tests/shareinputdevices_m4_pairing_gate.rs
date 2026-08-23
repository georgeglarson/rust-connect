//! Integration tests for the pairing-gate lane (Task #1042 panel M4
//! round 3 fix — security headline). The `capable_consumer_ids`
//! wrapper at `src/plugins/shareinputdevices/mod.rs` is the single
//! source of truth for "is this device a relay target"; both the
//! activation gate and the wire consumer's fan-out filter read it.
//! These tests pin the wrapper's discrimination through the same
//! shape the production code uses (a real `ConnectionManager` and
//! `PairingHandler`, fake-connected peers via `mark_fake_connected`).
//!
//! **Coverage:**
//! - Single-cap peer excluded even when paired (the AND shape is
//!   load-bearing — a phone that advertises only one of the two
//!   consumer caps cannot consume both arms, so it is never a
//!   relay target).
//! - Both caps + paired → included.
//! - Both caps + unpaired → excluded (the SECURITY headline).
//!
//! These tests do NOT require dbus-daemon: the wrapper's filter
//! logic operates on the CM + pairing-handle state directly, with no
//! bus calls. The plugin's gate spawn is unused here — these tests
//! only need the wrapper to return the right set given the right
//! peer state.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::Arc;

use rust_connect::plugins::shareinputdevices::{capable_consumer_ids, CONSUMER_INCOMING_CAPS};
use rust_connect::protocol::pairing::PairingHandler;
use rust_connect::protocol::CertificateManager;
use rust_connect::protocol::ConnectionManager;

async fn build_cm_and_pairing() -> (Arc<ConnectionManager>, Arc<PairingHandler>) {
    let cert_dir = tempfile::tempdir().expect("cert tempdir");
    let cert_manager = Arc::new(CertificateManager::new(cert_dir.path().to_path_buf()));
    let cm =
        Arc::new(ConnectionManager::new(cert_manager.clone()).expect("ConnectionManager::new"));
    let pairing = Arc::new(PairingHandler::new(cert_manager));
    (cm, pairing)
}

async fn record_full_consumer_caps(cm: &ConnectionManager, device_id: &str) {
    cm.record_peer_capabilities(
        &device_id.to_string(),
        &[
            CONSUMER_INCOMING_CAPS[0].to_string(),
            CONSUMER_INCOMING_CAPS[1].to_string(),
        ],
        &["kdeconnect.ping".to_string()],
    )
    .await;
    cm.mark_fake_connected_for_test(device_id);
}

async fn record_only_shareinputdevices_cap(cm: &ConnectionManager, device_id: &str) {
    cm.record_peer_capabilities(
        &device_id.to_string(),
        &[CONSUMER_INCOMING_CAPS[0].to_string()],
        &["kdeconnect.ping".to_string()],
    )
    .await;
    cm.mark_fake_connected_for_test(device_id);
}

async fn record_only_mousepad_cap(cm: &ConnectionManager, device_id: &str) {
    cm.record_peer_capabilities(
        &device_id.to_string(),
        &[CONSUMER_INCOMING_CAPS[1].to_string()],
        &["kdeconnect.ping".to_string()],
    )
    .await;
    cm.mark_fake_connected_for_test(device_id);
}

/// Stage a fake peer certificate so the cert-anchor gate added by PR #28
/// (accept refuses without a pending cert or pinned fingerprint) lets
/// the accept through. Mirrors the orchestrator's `make_external_cert_der`.
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

async fn fake_pair(pairing: &PairingHandler, device_id: &str) {
    pairing
        .initiate_pairing(&device_id.to_string())
        .await
        .expect("initiate_pairing must succeed");
    // PR #28's cert-anchor gate: accept refuses without a pending cert.
    stage_peer_cert(pairing, device_id).await;
    pairing
        .accept_pairing(&device_id.to_string())
        .await
        .expect("accept_pairing must succeed");
}

/// Both caps + paired → wrapper includes the peer.
///
/// **Red-before-green.** The wrapper previously took no pairing
/// argument, so the paired/unpaired distinction did not exist; this
/// test only compiles after the pairing-handler argument is added
/// to `capable_consumer_ids`.
#[tokio::test]
async fn wrapper_includes_paired_peer_with_both_caps() {
    let (cm, pairing) = build_cm_and_pairing().await;

    let peer = "peer-both-caps-pairedaaaaaaaaaaa";
    record_full_consumer_caps(&cm, peer).await;
    fake_pair(&pairing, peer).await;

    let result = capable_consumer_ids(&cm, Some(&pairing)).await;
    assert!(
        result.contains(&peer.to_string()),
        "both-caps + paired peer must be a consumer candidate; got {:?}",
        result
    );
}

/// Both caps + UNPAIRED → wrapper excludes the peer. THE SECURITY
/// HEADLINE: an unpaired peer that has completed the TLS handshake
/// advertising the consumer caps is NOT a relay target.
///
/// **Red-before-green.** The pre-fix wrapper included the peer
/// (capability + connection-existence only); the pairing predicate
/// was added by the round-3 fix and this test pins it.
#[tokio::test]
async fn wrapper_excludes_unpaired_peer_with_both_caps() {
    let (cm, pairing) = build_cm_and_pairing().await;

    let peer = "peer-both-caps-unpairedaaaaaaaaa";
    record_full_consumer_caps(&cm, peer).await;
    // Deliberately no `fake_pair` — the security case under test.

    let result = capable_consumer_ids(&cm, Some(&pairing)).await;
    assert!(
        !result.contains(&peer.to_string()),
        "both-caps + UNPAIRED peer must NOT be a consumer candidate (security headline); \
         got {:?}",
        result
    );
}

/// Single-cap peer excluded even when paired.
///
/// **Red-before-green.** The pre-fix wrapper inherited the OR shape
/// from `cm.capable_consumer_ids`; a peer advertising only the
/// shareinputdevices.request cap (or only the mousepad.request
/// cap) qualified as a consumer even though an activated session
/// emits BOTH packet types — the trapped-cursor shape the round-2
/// P5 fix closed at the wrapper level. This test pins the AND
/// shape through the same wrapper the gate and the consumer read.
#[tokio::test]
async fn wrapper_excludes_paired_peer_with_single_cap() {
    let (cm, pairing) = build_cm_and_pairing().await;

    let only_share = "peer-only-shareinputdevicesaaaaa";
    record_only_shareinputdevices_cap(&cm, only_share).await;
    fake_pair(&pairing, only_share).await;

    let result = capable_consumer_ids(&cm, Some(&pairing)).await;
    assert!(
        !result.contains(&only_share.to_string()),
        "peer advertising only shareinputdevices.request (not mousepad.request) must be \
         excluded even when paired (AND-match invariant); got {:?}",
        result
    );

    let only_mouse = "peer-only-mousepadaaaaaaaaaaaaaa";
    record_only_mousepad_cap(&cm, only_mouse).await;
    fake_pair(&pairing, only_mouse).await;

    let result = capable_consumer_ids(&cm, Some(&pairing)).await;
    assert!(
        !result.contains(&only_mouse.to_string()),
        "peer advertising only mousepad.request (not shareinputdevices.request) must be \
         excluded even when paired (AND-match invariant); got {:?}",
        result
    );
}

/// Three-peer mixture: both-caps-paired is IN, both-caps-unpaired is
/// OUT, single-cap-paired is OUT. Pins all three discriminators in
/// one snapshot — the realistic shape of a LAN with a paired phone,
/// a freshly-handed-off phone, and a stale-cap phone.
#[tokio::test]
async fn wrapper_handles_mixed_peer_state() {
    let (cm, pairing) = build_cm_and_pairing().await;

    let paired = "peer-mixed-pairedaaaaaaaaaaaaaaa";
    let unpaired = "peer-mixed-unpairedaaaaaaaaaaaaa";
    let single_cap = "peer-mixed-single-capaaaaaaaaaaa";

    record_full_consumer_caps(&cm, paired).await;
    fake_pair(&pairing, paired).await;

    record_full_consumer_caps(&cm, unpaired).await;
    // No fake_pair.

    record_only_shareinputdevices_cap(&cm, single_cap).await;
    fake_pair(&pairing, single_cap).await;

    let mut result = capable_consumer_ids(&cm, Some(&pairing)).await;
    result.sort();

    assert_eq!(
        result,
        vec![paired.to_string()],
        "mixed snapshot must include only both-caps-paired; got {:?}",
        result
    );
}
