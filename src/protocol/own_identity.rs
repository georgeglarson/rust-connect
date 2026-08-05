//! Own identity: the daemon's single source of truth for who it is.
//!
//! Historically the daemon kept its identity in THREE unsynchronized places
//! (the `device_id` file, `ConnectionManager.device_id`, and a readdir scan
//! of the cert dir picking the first non-`_peer.crt`). On 2026-04-07 that
//! let a peer's device id occupy the own slot: the daemon minted its own
//! keypair under a peer's id and broadcast the phone's id as its own, which
//! the phone then hid as a self-duplicate. `OwnIdentity::load_or_create`
//! makes that state structurally unreachable:
//!
//! - the own certificate lives at fixed paths (`own.crt`/`own.key`), never
//!   keyed by any device id;
//! - the stored cert's CN and the `device_id` file must agree — if they
//!   don't, the identity is unpairable-by-construction, so a fresh identity
//!   is generated loudly instead of limping along;
//! - legacy `<device_id>.crt/.key` layouts are adopted only when the CN
//!   matches.

use std::path::Path;

use tracing::{info, warn};

use crate::device::DeviceId;
use crate::protocol::crypto::{validate_device_id, CertificateManager};
use crate::protocol::types::Identity;
use crate::utils::errors::{Error, Result};

#[derive(Debug, Clone)]
pub struct OwnIdentity {
    pub device_id: DeviceId,
    pub device_name: String,
}

impl OwnIdentity {
    /// Load the daemon identity from disk, creating or repairing it as
    /// needed. This is the ONLY code path that may create an own identity.
    pub fn load_or_create(
        cert_manager: &CertificateManager,
        data_dir: &Path,
        device_name: &str,
    ) -> Result<Self> {
        let id_path = data_dir.join("device_id");
        let id_from_file = std::fs::read_to_string(&id_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| validate_device_id(s).is_ok());

        let cn_from_cert = cert_manager.own_certificate_cn()?;

        let device_id = match (id_from_file, cn_from_cert) {
            (Some(id), Some(cn)) if id == cn => id,
            (Some(id), Some(cn)) => {
                // The April clobber state: file and cert disagree. An identity
                // whose id and cert CN differ can never complete pairing, so
                // start fresh — loudly.
                warn!(
                    file_id = %id,
                    cert_cn = %cn,
                    "device_id file and own certificate CN disagree — generating a FRESH identity (stale pairings must be re-established)"
                );
                Self::regenerate(cert_manager, &id_path, device_name)?
            }
            (Some(id), None) => {
                // Have an id, no own cert yet: adopt a legacy layout if the
                // CN matches, otherwise mint a fresh cert for this id.
                if !cert_manager.adopt_legacy_own_certificate(&id)? {
                    cert_manager.ensure_own_certificate(&id, device_name)?;
                }
                id
            }
            (None, Some(cn)) => {
                // Cert is the stronger artifact; recover the id from its CN.
                info!(device_id = %cn, "Recovered device id from own certificate CN");
                Self::write_id_file(&id_path, &cn)?;
                cn
            }
            (None, None) => Self::regenerate(cert_manager, &id_path, device_name)?,
        };

        info!(device_id = %device_id, "Using device identity");
        Ok(Self {
            device_id,
            device_name: device_name.to_string(),
        })
    }

    fn regenerate(
        cert_manager: &CertificateManager,
        id_path: &Path,
        device_name: &str,
    ) -> Result<DeviceId> {
        let id = Identity::generate_device_id();
        // A stale own cert (if any) no longer matches; replace it outright.
        let (cert_pem, key_pem) = cert_manager.generate_certificate(&id, device_name)?;
        cert_manager.store_own_certificate(&cert_pem, &key_pem)?;
        Self::write_id_file(id_path, &id)?;
        info!(device_id = %id, "Generated fresh device identity");
        Ok(id)
    }

    fn write_id_file(id_path: &Path, device_id: &str) -> Result<()> {
        std::fs::write(id_path, device_id).map_err(|e| {
            Error::io(
                format!("Failed to write device_id file: {}", e),
                Some(id_path.display().to_string()),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    fn setup() -> (CertificateManager, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cm = CertificateManager::new(temp_dir.path().join("certs"));
        cm.init().unwrap();
        (cm, temp_dir)
    }

    #[test]
    fn fresh_dir_creates_identity_and_is_stable() {
        let (cm, temp) = setup();
        let first = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        assert!(validate_device_id(&first.device_id).is_ok());
        assert!(cm.has_own_certificate());
        let second = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        assert_eq!(first.device_id, second.device_id);
    }

    #[test]
    fn cert_cn_recovers_missing_id_file() {
        let (cm, temp) = setup();
        let first = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        std::fs::remove_file(temp.path().join("device_id")).unwrap();
        let second = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        assert_eq!(first.device_id, second.device_id);
        assert!(temp.path().join("device_id").exists());
    }

    #[test]
    fn mismatched_file_and_cert_regenerates_fresh_identity() {
        let (cm, temp) = setup();
        let first = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        // Simulate the clobber: file claims a different (valid) id.
        let other_id = "b".repeat(32);
        std::fs::write(temp.path().join("device_id"), &other_id).unwrap();
        let second = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        assert_ne!(second.device_id, first.device_id);
        assert_ne!(second.device_id, other_id);
        // New identity must be self-consistent: CN == file.
        assert_eq!(
            cm.own_certificate_cn().unwrap().as_deref(),
            Some(second.device_id.as_str())
        );
    }

    #[test]
    fn legacy_cert_with_matching_cn_is_adopted() {
        let (cm, temp) = setup();
        let id = "c".repeat(32);
        cm.ensure_certificate(&id, "Test").unwrap();
        std::fs::write(temp.path().join("device_id"), &id).unwrap();
        let identity = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        assert_eq!(identity.device_id, id);
        assert!(cm.has_own_certificate());
        // Legacy slot was consumed by the rename.
        assert!(!temp
            .path()
            .join("certs")
            .join(format!("{}.crt", id))
            .exists());
    }

    #[test]
    fn legacy_cert_with_mismatched_cn_is_not_adopted() {
        let (cm, temp) = setup();
        let legacy_id = "d".repeat(32);
        cm.ensure_certificate(&legacy_id, "Test").unwrap();
        let other_id = "e".repeat(32);
        std::fs::write(temp.path().join("device_id"), &other_id).unwrap();
        let identity = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        // A fresh cert for other_id is minted; the legacy relic is left alone.
        assert_eq!(identity.device_id, other_id);
        assert!(temp
            .path()
            .join("certs")
            .join(format!("{}.crt", legacy_id))
            .exists());
    }

    #[test]
    fn invalid_id_file_is_replaced() {
        let (cm, temp) = setup();
        std::fs::write(temp.path().join("device_id"), "too-short").unwrap();
        let identity = OwnIdentity::load_or_create(&cm, temp.path(), "Test").unwrap();
        assert!(validate_device_id(&identity.device_id).is_ok());
    }
}
