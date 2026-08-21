//! Pairing handler for KDE Connect protocol
//!
//! Single Responsibility: Manage device pairing lifecycle.
//! Does NOT handle connections, discovery, or packet routing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::device::types::DeviceId;
use crate::protocol::crypto::CertificateManager;
use crate::protocol::types::PairingRequest;
use crate::utils::errors::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    NotPaired,
    Requested,
    RequestedByPeer,
    Paired,
}

impl PairState {
    /// Stable string form for API responses.
    pub fn as_api_str(&self) -> &'static str {
        match self {
            PairState::NotPaired => "not_paired",
            PairState::Requested => "requested",
            PairState::RequestedByPeer => "requested_by_peer",
            PairState::Paired => "paired",
        }
    }
}

/// Requester-side pairing timeout: how long we wait for the peer's accept
/// (Android PairingHandler.kt:151 — 30 seconds).
pub const PAIR_REQUEST_TIMEOUT_SECS: i64 = 30;

/// Accepter-side pairing timeout: how long the peer's request stays pending
/// waiting for the local user's response (Android PairingHandler.kt:88 —
/// 25 seconds; the requester times out at 30, so the accepter gives up first).
pub const PAIR_ACCEPT_TIMEOUT_SECS: i64 = 25;

pub struct PairingHandler {
    cert_manager: Arc<CertificateManager>,
    outgoing: Arc<RwLock<HashMap<DeviceId, PairingRequest>>>,
    incoming: Arc<RwLock<HashMap<DeviceId, PairingRequest>>>,
    paired: Arc<RwLock<HashMap<DeviceId, DateTime<Utc>>>>,
    /// Peer certs captured at connection time, held ONLY while a pairing is
    /// pending. Verify-before-write: the cert reaches disk
    /// (`store_peer_certificate`) exclusively inside accept_pairing, after
    /// the pairing is confirmed — never at request time.
    pending_certs: Arc<RwLock<HashMap<DeviceId, Vec<u8>>>>,
    /// Requester-side timeout (outgoing requests).
    timeout: Duration,
    /// Accepter-side timeout (incoming requests).
    accept_timeout: Duration,
    max_pending: usize,
    persist_path: Option<PathBuf>,
    /// The daemon's own device id, when known. A peer_id equal to this is
    /// refused at every path in this module that can originate pairing
    /// state (`initiate_pairing`, `receive_pair_request`,
    /// `force_accept_pairing`), and `set_own_device_id` prunes any matching
    /// entry already sitting in `paired` from a disk load that predates the
    /// id being known (see its doc comment).
    ///
    /// This is defense-in-depth BEHIND three existing connection-layer
    /// self-guards (landed 2026-07-29) that are NOT redundant
    /// with it: `protocol/discovery.rs:185-186` (ignore our own
    /// broadcasts), `services/connection_orchestrator.rs:207-219` (never
    /// dial our own identity), `protocol/connection/inbound.rs:93-101`
    /// (reject an incoming connection claiming our id). Those stop a live
    /// remote peer from ever reaching pairing code; none of them touch
    /// `paired.json`, which only this module writes. Don't remove any of
    /// them as "covered by the pairing guard now" — that guard is what
    /// actually stops a network peer; this one covers persisted state.
    ///
    /// Behind a lock (not a plain field) because the real device id isn't
    /// known until `Daemon::load_identity` runs, which is after
    /// `AppState::new` has already wrapped this handler in `Arc` —
    /// `set_own_device_id` is the production write path, mirroring
    /// `ConnectionManager::set_device_identity`. `None` only when a caller
    /// genuinely hasn't wired the identity in yet (e.g. older test setups).
    own_device_id: Arc<RwLock<Option<DeviceId>>>,
}

impl PairingHandler {
    pub fn new(cert_manager: Arc<CertificateManager>) -> Self {
        Self {
            cert_manager,
            outgoing: Arc::new(RwLock::new(HashMap::new())),
            incoming: Arc::new(RwLock::new(HashMap::new())),
            paired: Arc::new(RwLock::new(HashMap::new())),
            pending_certs: Arc::new(RwLock::new(HashMap::new())),
            timeout: Duration::seconds(PAIR_REQUEST_TIMEOUT_SECS),
            accept_timeout: Duration::seconds(PAIR_ACCEPT_TIMEOUT_SECS),
            max_pending: 10,
            persist_path: None,
            own_device_id: Arc::new(RwLock::new(None)),
        }
    }

    pub fn with_persistence(mut self, path: PathBuf) -> Self {
        self.persist_path = Some(path);
        self
    }

    /// Wire in the daemon's own device id at construction time (tests, or
    /// any caller that knows its id before the handler is shared). See the
    /// `own_device_id` field doc for why this matters.
    pub fn with_own_device_id(self, device_id: DeviceId) -> Self {
        Self {
            own_device_id: Arc::new(RwLock::new(Some(device_id))),
            ..self
        }
    }

    /// Wire in the daemon's own device id after construction — the
    /// production path, called from `Daemon::load_identity` once the id is
    /// known, alongside `ConnectionManager::set_device_identity`.
    ///
    /// Also prunes a `paired` entry keyed to this id, if one exists. This is
    /// the only point in the daemon's life where that invariant can actually
    /// be enforced: `load_from_disk` (bootstrap.rs, inside `create_state`)
    /// runs strictly before this — before the id is known — so it has no
    /// way to filter such an entry out at load time (initial-load review,
    /// Important-1). A self-keyed entry can only come from `paired.json`
    /// predating this guard, a restored backup, or a rollback; nothing that
    /// runs after this guard exists can produce one.
    pub async fn set_own_device_id(&self, device_id: DeviceId) {
        *self.own_device_id.write().await = Some(device_id.clone());

        let had_self_entry = {
            let mut paired = self.paired.write().await;
            paired.remove(&device_id).is_some()
        };

        if had_self_entry {
            warn!(
                device_id = %device_id,
                event = "self_keyed_paired_entry_pruned",
                "Dropped a paired-device entry keyed to our own device id (stale paired.json, a restored backup, or a rollback)"
            );
            if let Err(e) = self.save_to_disk().await {
                warn!(error = %e, device_id = %device_id, event = "save_after_self_prune_failed", "Failed to persist pairing state after pruning self-keyed entry");
            }
        }
    }

    async fn is_own_device_id(&self, device_id: &DeviceId) -> bool {
        self.own_device_id.read().await.as_ref() == Some(device_id)
    }

    /// Clone of the shared `paired` handle — the SAME `Arc`, not a copy of
    /// its contents. `DeviceRegistry` holds this (wired via
    /// `DeviceRegistry::with_paired_source` in app.rs) so it can tell
    /// truly-paired devices (completed SAS pairing, present in this map)
    /// apart from records that merely reached `Connected`
    /// (`Device::is_paired`'s weaker, state-based signal) — finding L2-1,
    /// Sprint 2 security audit. Sharing the inner `Arc` here — never
    /// `Arc<PairingHandler>` itself — keeps the registry from holding a
    /// back-reference to this handler, so there is no reference cycle.
    pub fn paired_handle(&self) -> Arc<RwLock<HashMap<DeviceId, DateTime<Utc>>>> {
        self.paired.clone()
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_accept_timeout(mut self, timeout: Duration) -> Self {
        self.accept_timeout = timeout;
        self
    }

    pub fn with_max_pending(mut self, max: usize) -> Self {
        self.max_pending = max;
        self
    }

    /// Record the peer's certificate (captured at TLS handshake time) for a
    /// pending pairing. Held in memory only; persisted on accept.
    pub async fn set_pending_peer_cert(&self, device_id: &DeviceId, cert_der: Vec<u8>) {
        self.pending_certs
            .write()
            .await
            .insert(device_id.clone(), cert_der);
    }

    /// Initiate an outgoing pairing request. Returns the v8 timestamp that
    /// MUST be transmitted in the pair packet — the SAS on both sides is
    /// computed over this exact value.
    pub async fn initiate_pairing(&self, device_id: &DeviceId) -> Result<i64> {
        if self.is_own_device_id(device_id).await {
            return Err(Error::PairingRejected(format!(
                "Refusing to initiate pairing with our own device id: {}",
                device_id
            )));
        }

        self.cleanup_expired().await?;

        {
            let paired = self.paired.read().await;
            if paired.contains_key(device_id) {
                return Err(Error::DeviceAlreadyExists(format!(
                    "Device {} is already paired",
                    device_id
                )));
            }
        }

        {
            let total = self.outgoing.read().await.len() + self.incoming.read().await.len();
            if total >= self.max_pending {
                return Err(Error::PairingRejected(format!(
                    "Too many pending pairing requests (max {})",
                    self.max_pending
                )));
            }
        }

        let request = PairingRequest {
            device_id: device_id.clone(),
            created_at: Utc::now(),
            timeout: self.timeout,
            pair_timestamp: Utc::now().timestamp(),
        };
        let pair_timestamp = request.pair_timestamp;

        let mut outgoing = self.outgoing.write().await;
        outgoing.insert(device_id.clone(), request);

        info!(
            device_id = %device_id,
            event = "pairing_initiated",
            "Initiated outgoing pairing request"
        );

        Ok(pair_timestamp)
    }

    /// Handle an incoming pair request. `timestamp` is the v8 timestamp from
    /// the peer's pair packet; it is recorded so the SAS matches the phone's
    /// byte-for-byte. A v8+ peer's request WITHOUT a timestamp is rejected
    /// by the caller (connection_loop gates the requirement on
    /// protocolVersion >= 8, PairingHandler.kt:71-83); a v7 peer never sends
    /// one, which is recorded as the 0 sentinel — no SAS is surfaced for v7
    /// (the SAS is a v8 feature).
    ///
    /// Already-paired peers: mirrors Android PairingHandler.kt:60-68 — a pair
    /// request from a device we already trust unpairs BOTH sides (Android
    /// does it to itself; we do it to ourselves) and starts over as a fresh
    /// request. Never silently re-confirm: answering with pair:true makes
    /// Android unpair us.
    ///
    /// Duplicate requests: mirrors Android PairingHandler.kt:53-58 — a second
    /// request while one is pending is IGNORED; the first (and its SAS
    /// timestamp) stands.
    pub async fn receive_pair_request(
        &self,
        device_id: &DeviceId,
        timestamp: Option<i64>,
    ) -> Result<()> {
        self.receive_pair_request_with_cert(device_id, timestamp, None)
            .await
    }

    /// `receive_pair_request` plus the peer cert for the new stretch. The
    /// cert is staged AFTER `cleanup_expired`: the sweep drops staged certs
    /// for the requests it reaps, so a cert staged before the
    /// call — the connection loop's natural order — can be wiped by the
    /// reap of THIS device's own dead stretch, and the mutual-initiation
    /// auto-accept below would then persist NOTHING (a silently cert-less
    /// pairing). Staging inside, after the sweep, keeps both invariants:
    /// dead-stretch certs never linger, fresh-stretch certs never wipe.
    pub async fn receive_pair_request_with_cert(
        &self,
        device_id: &DeviceId,
        timestamp: Option<i64>,
        cert_der: Option<Vec<u8>>,
    ) -> Result<()> {
        if self.is_own_device_id(device_id).await {
            warn!(
                device_id = %device_id,
                event = "pair_request_self_id_refused",
                "Refusing pair request claiming our own device id"
            );
            return Err(Error::PairingRejected(format!(
                "Refusing pair request claiming our own device id: {}",
                device_id
            )));
        }

        if self.is_paired(device_id).await {
            info!(
                device_id = %device_id,
                event = "pair_request_while_paired",
                "Pair request from already-paired device; unpairing and treating as fresh request (Android semantics)"
            );
            let _ = self.unpair(device_id).await;
        }

        self.cleanup_expired().await?;

        {
            let outgoing = self.outgoing.read().await;
            if outgoing.contains_key(device_id) {
                info!(
                    device_id = %device_id,
                    event = "pair_request_auto_accept",
                    "Received pair request from device we also initiated, auto-accepting"
                );
                drop(outgoing);
                // Stage the fresh cert for the accept to persist. Both
                // staging points in this method are (a) after the sweep —
                // cleanup_expired drops staged certs for reaped requests,
                // and this device's own dead stretch may have been among
                // them — and (b) only once the request is known live: a
                // cert staged for a request that is then rejected
                // (duplicate, max-pending) would orphan in pending_certs
                // with no expiry path to reap it.
                if let Some(cert_der) = cert_der {
                    self.set_pending_peer_cert(device_id, cert_der).await;
                }
                return self.accept_pairing(device_id).await;
            }
        }

        {
            let incoming = self.incoming.read().await;
            if incoming.contains_key(device_id) {
                info!(
                    device_id = %device_id,
                    event = "pair_request_duplicate_ignored",
                    "Ignoring second pairing request before the first one timed out"
                );
                return Ok(());
            }
        }

        let total = self.outgoing.read().await.len() + self.incoming.read().await.len();
        if total >= self.max_pending {
            return Err(Error::PairingRejected(format!(
                "Too many pending pairing requests (max {})",
                self.max_pending
            )));
        }

        // v7 peers send no timestamp; 0 marks "absent" (see doc comment).
        let pair_timestamp = timestamp.unwrap_or(0);

        let request = PairingRequest {
            device_id: device_id.clone(),
            created_at: Utc::now(),
            timeout: self.accept_timeout,
            pair_timestamp,
        };

        {
            let mut incoming = self.incoming.write().await;
            incoming.insert(device_id.clone(), request);
        }

        // Second staging point: the request now exists, so the cert has an
        // expiry path and cannot orphan (see the auto-accept arm above).
        if let Some(cert_der) = cert_der {
            self.set_pending_peer_cert(device_id, cert_der).await;
        }

        // Surface the SAS in the journal at request time: every accept
        // surface (CLI, API, UI) displays it, and the log is the fallback
        // record if one does not.
        if let Ok(Some(sas)) = self.get_verification_key(device_id).await {
            info!(
                device_id = %device_id,
                verification_key = %sas,
                event = "pair_request_sas",
                "Incoming pairing request — compare this verification key on both devices"
            );
        }

        info!(
            device_id = %device_id,
            event = "pair_request_received",
            "Received incoming pairing request, awaiting user acceptance"
        );

        Ok(())
    }

    async fn remove_pending_request(&self, device_id: &DeviceId) -> Result<bool> {
        // The map guards are taken and dropped BEFORE any pending_certs
        // await: an `if let` scrutinee temporary lives for the whole body,
        // so `if let Some(req) = self.outgoing.write().await.remove(..)`
        // would hold the map write guard across pending_certs.write().await
        // in the expiry arm — reverse acquisition order vs the other
        // cert-drop sites and an AB-BA deadlock risk.
        let outgoing_removed = {
            let mut outgoing = self.outgoing.write().await;
            outgoing.remove(device_id)
        };
        if let Some(request) = outgoing_removed {
            if request.is_expired() {
                warn!(
                    device_id = %device_id,
                    event = "pairing_expired",
                    "Outgoing pairing request has expired"
                );
                // The peer cert staged at TLS handshake belongs to the
                // pairing that just died: verify-before-write persists it
                // only at accept, which expiry forecloses, so it is inert
                // and must not linger. A fresh request re-stages
                // the current cert before any accept can run — after any
                // sweep, never before it (receive_pair_request_with_cert
                // stages post-cleanup_expired; the API accept path stages
                // immediately before accept_pairing).
                self.pending_certs.write().await.remove(device_id);
                return Err(Error::PairingTimeout(device_id.clone()));
            }
            return Ok(true);
        }

        let incoming_removed = {
            let mut incoming = self.incoming.write().await;
            incoming.remove(device_id)
        };
        if let Some(request) = incoming_removed {
            if request.is_expired() {
                warn!(
                    device_id = %device_id,
                    event = "pairing_expired",
                    "Incoming pairing request has expired"
                );
                self.pending_certs.write().await.remove(device_id);
                return Err(Error::PairingTimeout(device_id.clone()));
            }
            return Ok(true);
        }

        Ok(false)
    }

    /// Identity anchor pre-flight (vk #1056): true when a pending peer
    /// cert is staged for this device (the happy path — captured at TLS
    /// handshake time) or a TOFU fingerprint is already pinned (the
    /// re-pair path). The anchor is the FINGERPRINT, not the PEM file:
    /// later handshakes enforce the pin via `has_peer_fingerprint`
    /// (verify_peer_certificate skips comparison when it is absent),
    /// so a PEM without a fingerprint — the mid-store crash window in
    /// store_peer_certificate — anchors nothing.
    ///
    /// Advisory, for callers that must decide BEFORE acting (the API
    /// handler's pre-send check). The authoritative gate lives inside
    /// accept_pairing / force_accept_pairing, which re-validate
    /// atomically against the cert they remove under one write lock.
    pub async fn has_identity_anchor(&self, device_id: &DeviceId) -> bool {
        self.pending_certs.read().await.contains_key(device_id)
            || self.cert_manager.has_peer_fingerprint(device_id)
    }

    pub async fn accept_pairing(&self, device_id: &DeviceId) -> Result<()> {
        let had_request = self.remove_pending_request(device_id).await?;

        if !had_request {
            return Err(Error::NoPendingPairRequest(device_id.clone()));
        }

        // Verify-before-write: the peer's certificate is persisted HERE, at
        // confirmation time, as the only write path — never at request time.
        // If the store fails (e.g. fingerprint mismatch against a stale
        // entry), the pairing does not complete.
        //
        // Remove-first, then gate on the REMOVED value (panel ae22e5d6):
        // take and check under one write lock, so a concurrent
        // reject_pairing that won the race leaves `None` here and the
        // gate refuses — a peek-then-remove gate would pass on a cert
        // that no longer exists.
        let pending_cert = self.pending_certs.write().await.remove(device_id);

        // Identity anchor gate (vk #1056): refuse the accept unless we
        // have some peer certificate to bind the pairing to. The anchor
        // is the staged cert just removed, or an already-pinned TOFU
        // FINGERPRINT (the re-pair path — the fingerprint, not the PEM,
        // is what later handshakes enforce). Without an anchor the
        // pairing would carry no fingerprint, the SAS would be
        // uncomputable (get_verification_key requires a peer cert), and
        // a later attacker spoofing this device id would face no pin
        // check.
        if pending_cert.is_none() && !self.cert_manager.has_peer_fingerprint(device_id) {
            warn!(
                device_id = %device_id,
                event = "pairing_accept_refused_no_peer_cert",
                "Refusing pairing accept: no peer certificate (pending or pinned) is available"
            );
            return Err(Error::PairingRejected(format!(
                "Refusing pairing with {}: no peer certificate (pending or pinned) was presented; \
                 cert-less pairings are not accepted",
                device_id
            )));
        }

        if let Some(cert_der) = pending_cert {
            self.cert_manager
                .store_peer_certificate(device_id, &cert_der)?;
        }

        let mut paired = self.paired.write().await;
        paired.insert(device_id.clone(), Utc::now());

        info!(
            device_id = %device_id,
            event = "pairing_accepted",
            "Pairing accepted"
        );

        drop(paired);
        if let Err(e) = self.save_to_disk().await {
            warn!(error = %e, device_id = %device_id, event = "save_after_accept_failed", "Failed to save pairing state after accept");
        }

        Ok(())
    }

    pub async fn force_accept_pairing(&self, device_id: &DeviceId) -> Result<()> {
        if self.is_own_device_id(device_id).await {
            return Err(Error::PairingRejected(format!(
                "Refusing to force-accept pairing with our own device id: {}",
                device_id
            )));
        }

        let _ = self.remove_pending_request(device_id).await;

        // Same identity anchor gate as accept_pairing (vk #1056),
        // same remove-first atomicity (panel ae22e5d6): force_accept
        // has no pending-request prerequisite, but the anchor
        // requirement is identical — a first-time peer must not be
        // admitted without one.
        let pending_cert = self.pending_certs.write().await.remove(device_id);
        if pending_cert.is_none() && !self.cert_manager.has_peer_fingerprint(device_id) {
            warn!(
                device_id = %device_id,
                event = "force_accept_pairing_refused_no_peer_cert",
                "Refusing force-accept: no peer certificate (pending or pinned) is available"
            );
            return Err(Error::PairingRejected(format!(
                "Refusing force-accept of pairing with {}: no peer certificate (pending or pinned) was presented; \
                 cert-less pairings are not accepted",
                device_id
            )));
        }

        if let Some(cert_der) = pending_cert {
            self.cert_manager
                .store_peer_certificate(device_id, &cert_der)?;
        }

        let mut paired = self.paired.write().await;
        paired.insert(device_id.clone(), Utc::now());

        info!(
            device_id = %device_id,
            event = "pairing_force_accepted",
            "Pairing accepted (auto-accept, no pending request required)"
        );

        drop(paired);
        if let Err(e) = self.save_to_disk().await {
            warn!(error = %e, device_id = %device_id, event = "save_after_accept_failed", "Failed to save pairing state after accept");
        }

        Ok(())
    }

    pub async fn reject_pairing(&self, device_id: &DeviceId) -> Result<()> {
        self.pending_certs.write().await.remove(device_id);
        let removed = {
            let mut outgoing = self.outgoing.write().await;
            outgoing.remove(device_id).is_some()
        };
        let removed_incoming = {
            let mut incoming = self.incoming.write().await;
            incoming.remove(device_id).is_some()
        };

        if removed || removed_incoming {
            info!(
                device_id = %device_id,
                event = "pairing_rejected",
                "Pairing rejected"
            );
            Ok(())
        } else {
            Err(Error::NoPendingPairRequest(device_id.clone()))
        }
    }

    pub async fn handle_pair_response(&self, device_id: &DeviceId, accepted: bool) -> Result<()> {
        if accepted {
            self.accept_pairing(device_id).await
        } else {
            self.reject_pairing(device_id).await
        }
    }

    pub async fn unpair(&self, device_id: &DeviceId) -> Result<()> {
        self.pending_certs.write().await.remove(device_id);
        let removed = {
            let mut paired = self.paired.write().await;
            paired.remove(device_id).is_some()
        };

        if removed {
            let _ = self.cert_manager.delete_certificate(device_id);
            let _ = self.cert_manager.delete_peer_certificate(device_id);
            info!(
                device_id = %device_id,
                event = "device_unpaired",
                "Device unpaired"
            );
            if let Err(e) = self.save_to_disk().await {
                warn!(error = %e, device_id = %device_id, event = "save_after_unpair_failed", "Failed to save pairing state after unpair");
            }
            Ok(())
        } else {
            Err(Error::DeviceNotPaired(device_id.clone()))
        }
    }

    pub async fn is_paired(&self, device_id: &DeviceId) -> bool {
        let paired = self.paired.read().await;
        paired.contains_key(device_id)
    }

    pub async fn pair_state(&self, device_id: &DeviceId) -> PairState {
        let paired = self.paired.read().await;
        if paired.contains_key(device_id) {
            return PairState::Paired;
        }
        drop(paired);

        // Expired pending requests do not count: an entry past its timeout
        // is logically NotPaired (Android's PairingHandler drops it). The
        // maps are cleaned lazily, so without this check a stale outgoing
        // request would keep reporting Requested and mis-classify the peer's
        // fresh request as the accept of our dead one (live failure vs a
        // test phone, 2026-07-30).
        let outgoing = self.outgoing.read().await;
        if let Some(req) = outgoing.get(device_id) {
            if !req.is_expired() {
                return PairState::Requested;
            }
        }
        drop(outgoing);

        let incoming = self.incoming.read().await;
        if let Some(req) = incoming.get(device_id) {
            if !req.is_expired() {
                return PairState::RequestedByPeer;
            }
        }

        PairState::NotPaired
    }

    /// The SAS (short authentication string) for a pending pairing, computed
    /// exactly as Android's `PairingHandler.getVerificationKey` does: sorted
    /// public-key DERs (larger first) + the v8 pair-packet timestamp, SHA-256,
    /// first 4 bytes, uppercase hex. Returns None when no pairing is pending
    /// (Android likewise returns null outside Requested/RequestedByPeer).
    pub async fn get_verification_key(&self, device_id: &DeviceId) -> Result<Option<String>> {
        // Expired pending requests surface NO key (same lazy-cleanup rule as
        // pair_state): a stale entry's SAS is worse than none — an instant
        // accept fired on it during test-phone live validation (2026-07-30) and
        // hit the expired request instead of the phone's fresh one.
        // pair_timestamp == 0 is the v7 "absent" sentinel (see
        // receive_pair_request) — v7 predates the SAS, so no key surfaces.
        let pair_timestamp = {
            let outgoing = self.outgoing.read().await;
            if let Some(req) = outgoing.get(device_id) {
                if req.is_expired() || req.pair_timestamp == 0 {
                    None
                } else {
                    Some(req.pair_timestamp)
                }
            } else {
                drop(outgoing);
                let incoming = self.incoming.read().await;
                incoming.get(device_id).and_then(|req| {
                    if req.is_expired() || req.pair_timestamp == 0 {
                        None
                    } else {
                        Some(req.pair_timestamp)
                    }
                })
            }
        };

        let Some(pair_timestamp) = pair_timestamp else {
            return Ok(None);
        };

        // Peer cert: the pending (not-yet-persisted) cert first, falling back
        // to an already-stored one.
        let peer_cert_der = {
            let pending = self.pending_certs.read().await;
            pending.get(device_id).cloned()
        };
        let peer_cert_der = match peer_cert_der {
            Some(der) => der,
            None => {
                let pem = self.cert_manager.load_peer_certificate_pem(device_id)?;
                super::crypto::pem_to_der(&pem)?
            }
        };

        let (our_cert_pem, _) = self.cert_manager.load_own_certificate()?;
        let our_cert_der = super::crypto::pem_to_der(&our_cert_pem)?;

        let our_pubkey = CertificateManager::extract_pubkey_der(&our_cert_der)?;
        let peer_pubkey = CertificateManager::extract_pubkey_der(&peer_cert_der)?;

        let key =
            CertificateManager::compute_verification_key(&our_pubkey, &peer_pubkey, pair_timestamp);
        Ok(Some(key))
    }

    pub async fn has_pending_request(&self, device_id: &DeviceId) -> bool {
        self.outgoing
            .read()
            .await
            .get(device_id)
            .is_some_and(|r| !r.is_expired())
            || self
                .incoming
                .read()
                .await
                .get(device_id)
                .is_some_and(|r| !r.is_expired())
    }

    /// If there's a non-expired outgoing pair request for `device_id`,
    /// return its pair-packet timestamp. Used by the connection orchestrator
    /// to re-send the pair_request packet on link re-establish — without
    /// this, an INITIATE pairing that lands while the link is down silently
    /// drops the packet (api/handlers/device.rs:245-258 only sends when
    /// `is_connected`), and the peer never sees the request.
    pub async fn pending_outgoing_timestamp(&self, device_id: &DeviceId) -> Option<i64> {
        self.outgoing.read().await.get(device_id).and_then(|r| {
            if r.is_expired() {
                None
            } else {
                Some(r.pair_timestamp)
            }
        })
    }

    pub async fn has_incoming_request(&self, device_id: &DeviceId) -> bool {
        self.incoming
            .read()
            .await
            .get(device_id)
            .is_some_and(|r| !r.is_expired())
    }

    pub async fn paired_devices(&self) -> Vec<DeviceId> {
        let paired = self.paired.read().await;
        paired.keys().cloned().collect()
    }

    pub async fn pending_devices(&self) -> Vec<DeviceId> {
        let mut devices: Vec<DeviceId> = self.outgoing.read().await.keys().cloned().collect();
        devices.extend(self.incoming.read().await.keys().cloned());
        devices
    }

    pub async fn incoming_requests(&self) -> Vec<DeviceId> {
        self.incoming.read().await.keys().cloned().collect()
    }

    pub async fn cleanup_expired(&self) -> Result<usize> {
        // A reaped request's staged peer cert goes with it: the cert belongs
        // to the dead pairing stretch, is inert once the request is gone
        // (verify-before-write persists only at accept), and must not linger
        // into a later stretch (the same invariant
        // remove_pending_request's expiry arms keep; this sweep is the other
        // lazy-expiry path, invoked from initiate_pairing /
        // receive_pair_request).
        let mut expired_ids: Vec<DeviceId> = Vec::new();

        let mut outgoing = self.outgoing.write().await;
        let before_out = outgoing.len();
        outgoing.retain(|id, req| {
            if req.is_expired() {
                debug!(device_id = %id, event = "outgoing_pairing_expired_cleaned", "Cleaned up expired outgoing pairing request");
                expired_ids.push(id.clone());
                false
            } else {
                true
            }
        });
        drop(outgoing);

        let mut incoming = self.incoming.write().await;
        let before_in = incoming.len();
        incoming.retain(|id, req| {
            if req.is_expired() {
                debug!(device_id = %id, event = "incoming_pairing_expired_cleaned", "Cleaned up expired incoming pairing request");
                expired_ids.push(id.clone());
                false
            } else {
                true
            }
        });

        let removed =
            (before_out - self.outgoing.read().await.len()) + (before_in - incoming.len());
        if removed > 0 {
            let mut certs = self.pending_certs.write().await;
            for id in &expired_ids {
                certs.remove(id);
            }
            info!(
                removed = removed,
                event = "expired_requests_cleaned",
                "Cleaned up expired pairing requests"
            );
        }

        Ok(removed)
    }

    pub async fn paired_since(&self, device_id: &DeviceId) -> Option<DateTime<Utc>> {
        let paired = self.paired.read().await;
        paired.get(device_id).copied()
    }

    pub async fn save_to_disk(&self) -> Result<()> {
        let path = match &self.persist_path {
            Some(p) => p,
            None => return Ok(()),
        };

        let paired = self.paired.read().await;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::io(
                    format!("Failed to create pairing directory: {}", e),
                    Some(parent.display().to_string()),
                )
            })?;
        }

        let json = serde_json::to_string_pretty(&*paired).map_err(|e| {
            Error::io(
                format!("Failed to serialize paired devices: {}", e),
                Some(path.display().to_string()),
            )
        })?;

        let temp_path = path.with_extension("tmp");
        std::fs::write(&temp_path, json).map_err(|e| {
            Error::io(
                format!("Failed to write paired devices: {}", e),
                Some(temp_path.display().to_string()),
            )
        })?;
        std::fs::rename(&temp_path, path).map_err(|e| {
            Error::io(
                format!("Failed to rename paired devices temp file: {}", e),
                Some(path.display().to_string()),
            )
        })?;

        debug!(
            device_count = paired.len(),
            path = %path.display(),
            "Saved paired devices to disk"
        );

        Ok(())
    }

    pub async fn load_from_disk(&self) -> Result<()> {
        let path = match &self.persist_path {
            Some(p) => p,
            None => return Ok(()),
        };

        if !path.exists() {
            debug!(
                path = %path.display(),
                "No paired devices file found, starting empty"
            );
            return Ok(());
        }

        let json = std::fs::read_to_string(path).map_err(|e| {
            Error::io(
                format!("Failed to read paired devices: {}", e),
                Some(path.display().to_string()),
            )
        })?;

        let loaded: HashMap<DeviceId, DateTime<Utc>> =
            serde_json::from_str(&json).map_err(|e| {
                Error::io(
                    format!("Failed to parse paired devices: {}", e),
                    Some(path.display().to_string()),
                )
            })?;

        let mut paired = self.paired.write().await;
        paired.extend(loaded);

        info!(
            device_count = paired.len(),
            path = %path.display(),
            "Loaded paired devices from disk"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests;
