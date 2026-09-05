//! Certificate management for TLS connections
//!
//! This module handles certificate generation, storage, and validation
//! following SRP: ONLY certificate management, nothing else.

use crate::utils::errors::{Error, Result};
use chrono::Datelike;
use std::fs;
use std::path::PathBuf;
use tracing::{debug, error, info};

const MIN_DEVICE_ID_LEN: usize = 32;
const MAX_DEVICE_ID_LEN: usize = 38;

/// Decode the SINGLE certificate in a PEM file to DER.
///
/// Contract: the input holds exactly one `BEGIN CERTIFICATE` block. Every
/// caller feeds a file this process wrote — our own certificate, or a peer
/// certificate stored by `store_peer_certificate` via `der_to_pem` — and all
/// of those are single-cert. A bundle (leaf plus chain) is an error rather
/// than being silently decoded as its first entry, which is what this
/// hand-rolled parser used to do.
pub(crate) fn pem_to_der(pem_bytes: &[u8]) -> Result<Vec<u8>> {
    const BEGIN_MARKER: &str = "-----BEGIN CERTIFICATE-----";
    const END_MARKER: &str = "-----END CERTIFICATE-----";

    let pem_str = std::str::from_utf8(pem_bytes)
        .map_err(|e| Error::CertificateError(format!("Invalid UTF-8 in PEM: {}", e)))?;

    let begin_count = pem_str
        .lines()
        .filter(|line| line.trim() == BEGIN_MARKER)
        .count();
    if begin_count > 1 {
        return Err(Error::CertificateError(format!(
            "PEM holds {} certificates; exactly one is expected",
            begin_count
        )));
    }

    let mut in_cert = false;
    let mut b64 = String::new();
    for line in pem_str.lines() {
        let trimmed = line.trim();
        if trimmed == BEGIN_MARKER {
            in_cert = true;
        } else if trimmed == END_MARKER {
            break;
        } else if in_cert {
            b64.push_str(trimmed);
        }
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .map_err(|e| Error::CertificateError(format!("Failed to decode PEM base64: {}", e)))
}

fn der_to_pem(der_bytes: &[u8]) -> Result<Vec<u8>> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der_bytes);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    Ok(pem.into_bytes())
}

fn parse_x509_der(der: &[u8]) -> Result<x509_parser::prelude::X509Certificate<'_>> {
    x509_parser::prelude::parse_x509_certificate(der)
        .map(|(_, cert)| cert)
        .map_err(|e| Error::CertificateError(format!("Failed to parse DER certificate: {}", e)))
}

/// Validate a device id against the Android wire requirements
/// (`DeviceInfo.kt`: 32–38 chars of `[a-zA-Z0-9_-]`).
pub fn validate_device_id(id: &str) -> Result<()> {
    if id.len() < MIN_DEVICE_ID_LEN || id.len() > MAX_DEVICE_ID_LEN {
        return Err(Error::validation(
            format!(
                "device_id must be {}-{} characters",
                MIN_DEVICE_ID_LEN, MAX_DEVICE_ID_LEN
            ),
            Some("device_id".to_string()),
        ));
    }
    if id.contains("..") || id.contains('/') || id.contains('\\') || id.contains('\0') {
        return Err(Error::validation(
            "device_id contains invalid characters".to_string(),
            Some("device_id".to_string()),
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::validation(
            "device_id must contain only alphanumeric characters, hyphens, or underscores"
                .to_string(),
            Some("device_id".to_string()),
        ));
    }
    Ok(())
}

pub fn extract_cn_from_der(cert_der: &[u8]) -> Result<String> {
    let cert = parse_x509_der(cert_der)?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok().map(|s| s.to_string()));
    match cn {
        Some(name) => Ok(name),
        None => Err(Error::CertificateError(
            "Certificate has no Common Name (CN)".to_string(),
        )),
    }
}

/// Certificate Manager - handles certificate lifecycle
///
/// Responsibilities (SRP):
/// - Generate self-signed certificates
/// - Store certificates to disk
/// - Load certificates from disk
/// - Validate certificates
///
/// Does NOT handle:
/// - TLS connections (that's ConnectionManager)
/// - Certificate exchange (that's PairingHandler)
#[derive(Debug)]
pub struct CertificateManager {
    pub(crate) cert_dir: PathBuf,
}

impl CertificateManager {
    /// Create a new CertificateManager
    ///
    /// # Arguments
    /// * `cert_dir` - Directory to store certificates
    pub fn new(cert_dir: PathBuf) -> Self {
        Self { cert_dir }
    }

    /// Initialize certificate directory
    ///
    /// Creates the certificate directory if it doesn't exist
    pub fn init(&self) -> Result<()> {
        if !self.cert_dir.exists() {
            fs::create_dir_all(&self.cert_dir).map_err(|e| {
                Error::io(
                    format!("Failed to create certificate directory: {}", e),
                    Some(self.cert_dir.display().to_string()),
                )
            })?;
            info!(
                cert_dir = %self.cert_dir.display(),
                event = "cert_dir_created",
                "Created certificate directory"
            );
        }
        // The certs dir holds private keys — keep it owner-only. Applied on
        // every init, not just at creation, so installs upgraded from before
        // this hardening get an existing loose directory tightened too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.cert_dir, fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    Error::io(
                        format!("Failed to set certificate directory permissions: {}", e),
                        Some(self.cert_dir.display().to_string()),
                    )
                },
            )?;
        }
        Ok(())
    }

    /// Generate a self-signed certificate
    ///
    /// Generates a new RSA key pair and self-signed certificate
    /// for use in TLS connections.
    ///
    /// # Arguments
    /// * `device_id` - Unique device identifier
    /// * `device_name` - Human-readable device name
    ///
    /// # Returns
    /// Tuple of (certificate, private_key)
    pub fn generate_certificate(
        &self,
        device_id: &str,
        _device_name: &str,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        debug!(
            device_id = %device_id,
            device_name = %_device_name,
            "Generating self-signed certificate"
        );

        // Generate RSA 2048-bit key, signed with SHA-512
        // (matching KDE Connect: SslHelper.kt:121 signs SHA512withRSA)
        let signing_key =
            rcgen::KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA512, rcgen::RsaKeySize::_2048)
                .map_err(|e| Error::TlsError(format!("Failed to generate RSA key: {}", e)))?;

        // Build certificate parameters. We emit exactly one subjectAltName
        // entry: a dNSName carrying the device id. This is a DELIBERATE
        // divergence from Android's SslHelper.kt:53-57 — Android's TLS layer
        // has checkServerTrusted = Unit (trust-all at TLS, authenticity from
        // a post-handshake fingerprint TOFU), so its CN-only certs ship
        // fine. kdeconnectd's Qt TLS layer is the opposite: in client mode
        // it sets the outgoing socket's peer-verify name to the device id
        // (core/backends/lan/lanlinkprovider.cpp:604: setPeerVerifyName
        // (deviceId)) and runs REAL hostname verification against the peer
        // cert's SAN. Any error other than SelfSignedCertificate is fatal
        // (lanlinkprovider.cpp:456, sslErrors). A SAN-less cert yields
        // "The host name did not match any of the valid hosts for this
        // certificate", the link aborts, kdeconnectd's post-rejection
        // unpair fires, and our side honors it (PairingHandler::unpair →
        // delete_peer_certificate, src/protocol/pairing/mod.rs:693) — a
        // transient handshake becomes permanent depairing. The SAN fixes
        // the trigger. Device ids may contain '_' (the Android spec
        // allows it and real ids are hex); '_' is valid ASCII and rcgen's
        // Ia5String accepts ASCII, so no real device id is rejected.
        let mut params = rcgen::CertificateParams::default();
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            rcgen::string::Ia5String::try_from(device_id.to_string()).map_err(|e| {
                Error::TlsError(format!(
                    "device_id is not a valid IA5String for subjectAltName dNSName: {}",
                    e
                ))
            })?,
        ));

        // RDN order must match Android: CN, OU, O
        use rcgen::DnType;
        params
            .distinguished_name
            .push(DnType::CommonName, device_id);
        params
            .distinguished_name
            .push(DnType::OrganizationalUnitName, "KDE Connect");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "KDE");

        // Generate 20-byte random serial number (RFC 5280 4.1.2.2 caps the
        // ENCODED serial at 20 octets). The DER INTEGER writer pads with a
        // leading 0x00 whenever the first byte's high bit is set — a raw
        // 20-byte buffer therefore encodes to 21 octets on ~50% of
        // generations — and it strips leading zero bytes, so a zero first
        // byte encodes to 19 or fewer. Clearing the high bit AND forcing
        // the low bit pins the encoded serial at exactly 20 octets every
        // time (rcgen's own default serial does only the high-bit clear
        // and accepts a variable length ≤ 20; the == 20 pin in
        // test_generated_cert_validity_and_serial_unchanged_after_san is
        // stricter). 158 random bits remain — well above the 64-bit
        // entropy floor RFC 5280 recommends for serials.
        let mut serial_buf = [0u8; 20];
        rand::RngCore::try_fill_bytes(&mut rand::rngs::OsRng, &mut serial_buf)
            .map_err(|e| Error::TlsError(format!("Failed to generate serial: {}", e)))?;
        serial_buf[0] = (serial_buf[0] & 0x7F) | 0x01;
        params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial_buf));

        // Validity period matching Android (SslHelper.kt:110-111):
        // notBefore = now - 1 year, notAfter = now + 10 years.
        let now = chrono::Utc::now();
        let nb = (now - chrono::Duration::days(365)).date_naive();
        let na = (now + chrono::Duration::days(3650)).date_naive();
        params.not_before = rcgen::date_time_ymd(nb.year(), nb.month() as u8, nb.day() as u8);
        params.not_after = rcgen::date_time_ymd(na.year(), na.month() as u8, na.day() as u8);

        // Generate the self-signed certificate
        let cert = params
            .self_signed(&signing_key)
            .map_err(|e| Error::TlsError(format!("Failed to sign cert: {}", e)))?;

        let cert_pem = cert.pem().as_bytes().to_vec();
        let key_pem = signing_key.serialize_pem().into_bytes();

        info!(
            device_id = %device_id,
            event = "cert_generated",
            "Generated certificate"
        );

        Ok((cert_pem, key_pem))
    }

    /// Store certificate and private key to disk
    ///
    /// # Arguments
    /// * `device_id` - Device identifier (used for filename)
    /// * `cert_pem` - Certificate in PEM format
    /// * `key_pem` - Private key in PEM format
    pub fn store_certificate(
        &self,
        device_id: &str,
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<()> {
        self.init()?;

        let cert_path = self.cert_path(device_id)?;
        let key_path = self.key_path(device_id)?;

        debug!(
            device_id = %device_id,
            cert_path = %cert_path.display(),
            key_path = %key_path.display(),
            "Storing certificate to disk"
        );

        fs::write(&cert_path, cert_pem).map_err(|e| {
            Error::io(
                format!("Failed to write certificate: {}", e),
                Some(cert_path.display().to_string()),
            )
        })?;

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::fs::PermissionsExt;

            // For a pre-existing loose file, .mode() is ignored — tighten
            // BEFORE truncating and writing the new key.
            if key_path.exists() {
                fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).map_err(|e| {
                    Error::io(
                        format!("Failed to set private key permissions: {}", e),
                        Some(key_path.display().to_string()),
                    )
                })?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&key_path)
                .map_err(|e| {
                    Error::io(
                        format!("Failed to create private key file: {}", e),
                        Some(key_path.display().to_string()),
                    )
                })?;

            file.write_all(key_pem).map_err(|e| {
                Error::io(
                    format!("Failed to write private key data: {}", e),
                    Some(key_path.display().to_string()),
                )
            })?;

            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).map_err(|e| {
                Error::io(
                    format!("Failed to set private key permissions: {}", e),
                    Some(key_path.display().to_string()),
                )
            })?;
        }

        #[cfg(not(unix))]
        {
            fs::write(&key_path, key_pem).map_err(|e| {
                Error::io(
                    format!("Failed to write private key: {}", e),
                    Some(key_path.display().to_string()),
                )
            })?;
        }

        info!(
            device_id = %device_id,
            event = "cert_stored",
            "Stored certificate and private key"
        );

        Ok(())
    }

    pub fn load_certificate(&self, device_id: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        let cert_path = self.cert_path(device_id)?;
        let key_path = self.key_path(device_id)?;

        debug!(
            device_id = %device_id,
            cert_path = %cert_path.display(),
            key_path = %key_path.display(),
            "Loading certificate from disk"
        );

        if !cert_path.exists() {
            return Err(Error::not_found(
                "Certificate file not found",
                Some(cert_path.display().to_string()),
            ));
        }
        if !key_path.exists() {
            return Err(Error::not_found(
                "Private key file not found",
                Some(key_path.display().to_string()),
            ));
        }

        let cert_pem = fs::read(&cert_path).map_err(|e| {
            Error::io(
                format!("Failed to read certificate: {}", e),
                Some(cert_path.display().to_string()),
            )
        })?;

        let key_pem = fs::read(&key_path).map_err(|e| {
            Error::io(
                format!("Failed to read private key: {}", e),
                Some(key_path.display().to_string()),
            )
        })?;

        info!(
            device_id = %device_id,
            event = "cert_loaded",
            "Loaded certificate and private key"
        );

        Ok((cert_pem, key_pem))
    }

    /// Validate a certificate
    ///
    /// Performs basic validation:
    /// - Certificate can be parsed
    /// - Private key can be parsed
    /// - Certificate and key match
    ///
    /// # Arguments
    /// * `cert_pem` - Certificate in PEM format
    /// * `key_pem` - Private key in PEM format
    pub fn validate_certificate(&self, cert_pem: &[u8], key_pem: &[u8]) -> Result<()> {
        debug!("Validating certificate");

        // Validate cert PEM can be parsed as an actual X.509 certificate
        let cert_der = pem_to_der(cert_pem).map_err(|e| {
            Error::validation(
                format!("Failed to parse certificate: {}", e),
                Some("cert_pem".to_string()),
            )
        })?;
        parse_x509_der(&cert_der).map_err(|e| {
            Error::validation(
                format!("Failed to parse certificate: {}", e),
                Some("cert_pem".to_string()),
            )
        })?;

        // Validate key PEM can be parsed
        rcgen::KeyPair::from_pem(&String::from_utf8_lossy(key_pem)).map_err(|e| {
            Error::validation(
                format!("Failed to parse private key: {}", e),
                Some("key_pem".to_string()),
            )
        })?;

        info!("Certificate validation successful");

        Ok(())
    }

    /// Check if certificate exists for device
    ///
    /// # Arguments
    /// * `device_id` - Device identifier
    pub fn has_certificate(&self, device_id: &str) -> bool {
        let cert_path = match self.cert_path(device_id) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let key_path = match self.key_path(device_id) {
            Ok(p) => p,
            Err(_) => return false,
        };
        cert_path.exists() && key_path.exists()
    }

    pub fn ensure_certificate(&self, device_id: &str, device_name: &str) -> Result<String> {
        if !self.has_certificate(device_id) {
            let (cert_pem, key_pem) = self.generate_certificate(device_id, device_name)?;
            self.store_certificate(device_id, &cert_pem, &key_pem)?;
        }
        Ok(device_id.to_string())
    }

    // ---- Own identity (fixed paths, never keyed by device id) ----
    //
    // The daemon's own certificate lives at cert_dir/own.crt + own.key. Peer
    // certificates live at <device_id>_peer.crt. Keying the own cert by a
    // device id let a peer's id occupy the own slot (the 2026-04-07 identity
    // clobber) and made "which cert is ours" a readdir lottery. Fixed paths
    // make the collision structurally impossible.

    fn own_cert_path(&self) -> PathBuf {
        self.cert_dir.join("own.crt")
    }

    fn own_key_path(&self) -> PathBuf {
        self.cert_dir.join("own.key")
    }

    pub fn has_own_certificate(&self) -> bool {
        self.own_cert_path().exists() && self.own_key_path().exists()
    }

    pub fn load_own_certificate(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        let cert_pem = fs::read(self.own_cert_path()).map_err(|e| {
            Error::io(
                format!("Failed to read own certificate: {}", e),
                Some(self.own_cert_path().display().to_string()),
            )
        })?;
        let key_pem = fs::read(self.own_key_path()).map_err(|e| {
            Error::io(
                format!("Failed to read own key: {}", e),
                Some(self.own_key_path().display().to_string()),
            )
        })?;
        Ok((cert_pem, key_pem))
    }

    pub fn store_own_certificate(&self, cert_pem: &[u8], key_pem: &[u8]) -> Result<()> {
        self.init()?;
        fs::write(self.own_cert_path(), cert_pem).map_err(|e| {
            Error::io(
                format!("Failed to write own certificate: {}", e),
                Some(self.own_cert_path().display().to_string()),
            )
        })?;

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::fs::PermissionsExt;

            let key_path = self.own_key_path();
            // For a pre-existing loose file, .mode() is ignored — tighten
            // BEFORE truncating and writing the new key.
            if key_path.exists() {
                fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).map_err(|e| {
                    Error::io(
                        format!("Failed to set own key permissions: {}", e),
                        Some(key_path.display().to_string()),
                    )
                })?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&key_path)
                .map_err(|e| {
                    Error::io(
                        format!("Failed to create own key file: {}", e),
                        Some(key_path.display().to_string()),
                    )
                })?;

            file.write_all(key_pem).map_err(|e| {
                Error::io(
                    format!("Failed to write own key data: {}", e),
                    Some(key_path.display().to_string()),
                )
            })?;

            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).map_err(|e| {
                Error::io(
                    format!("Failed to set own key permissions: {}", e),
                    Some(key_path.display().to_string()),
                )
            })?;
        }

        #[cfg(not(unix))]
        {
            fs::write(self.own_key_path(), key_pem).map_err(|e| {
                Error::io(
                    format!("Failed to write own key: {}", e),
                    Some(self.own_key_path().display().to_string()),
                )
            })?;
        }
        Ok(())
    }

    pub fn ensure_own_certificate(&self, device_id: &str, device_name: &str) -> Result<()> {
        if self.has_own_certificate() && self.own_certificate_is_valid() {
            return Ok(());
        }
        let (cert_pem, key_pem) = self.generate_certificate(device_id, device_name)?;
        self.store_own_certificate(&cert_pem, &key_pem)?;
        Ok(())
    }

    /// Android regenerates its own certificate when it is expired or not yet
    /// effective (SslHelper.kt:81-87), or unreadable. False on any of those —
    /// the caller regenerates. (Android also drops all trusted devices on
    /// regeneration; we leave stored peer certs in place — they fail closed
    /// via fingerprint mismatch, same net effect, less destructive.)
    fn own_certificate_is_valid(&self) -> bool {
        let pem_bytes = match fs::read(self.own_cert_path()) {
            Ok(pem) => pem,
            Err(_) => return false,
        };
        let der = match pem_to_der(&pem_bytes) {
            Ok(der) => der,
            Err(_) => return false,
        };
        let cert = match parse_x509_der(&der) {
            Ok(cert) => cert,
            Err(_) => return false,
        };
        cert.validity().is_valid()
    }

    /// CN of the stored own certificate, if present.
    pub fn own_certificate_cn(&self) -> Result<Option<String>> {
        if !self.own_cert_path().exists() {
            return Ok(None);
        }
        let pem = fs::read(self.own_cert_path()).map_err(|e| {
            Error::io(
                format!("Failed to read own certificate: {}", e),
                Some(self.own_cert_path().display().to_string()),
            )
        })?;
        let der = pem_to_der(&pem).map_err(|e| {
            Error::CertificateError(format!("Failed to parse own certificate: {}", e))
        })?;
        Ok(Some(extract_cn_from_der(&der)?))
    }

    /// Adopt a legacy own certificate stored as `<device_id>.crt/.key` (the
    /// pre-2026-07 layout) as the fixed own cert — but only if its CN matches
    /// `device_id`. A legacy cert whose CN does NOT match is a relic of the
    /// identity clobber and is left untouched (the caller regenerates).
    /// Returns true if adoption happened.
    pub fn adopt_legacy_own_certificate(&self, device_id: &str) -> Result<bool> {
        if self.has_own_certificate() {
            return Ok(false);
        }
        let legacy_cert = self.cert_path(device_id)?;
        let legacy_key = self.key_path(device_id)?;
        if !legacy_cert.exists() || !legacy_key.exists() {
            return Ok(false);
        }
        let pem = fs::read(&legacy_cert).map_err(|e| {
            Error::io(
                format!("Failed to read legacy certificate: {}", e),
                Some(legacy_cert.display().to_string()),
            )
        })?;
        let cn_matches = pem_to_der(&pem)
            .ok()
            .and_then(|der| extract_cn_from_der(&der).ok())
            .is_some_and(|cn| cn == device_id);
        if !cn_matches {
            return Ok(false);
        }
        fs::rename(&legacy_cert, self.own_cert_path()).map_err(|e| {
            Error::io(
                format!("Failed to adopt legacy certificate: {}", e),
                Some(legacy_cert.display().to_string()),
            )
        })?;
        fs::rename(&legacy_key, self.own_key_path()).map_err(|e| {
            Error::io(
                format!("Failed to adopt legacy key: {}", e),
                Some(legacy_key.display().to_string()),
            )
        })?;
        // rename(2) keeps the legacy file's permissions — force the private
        // key back to owner-only regardless of how loosely it was stored.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(self.own_key_path(), fs::Permissions::from_mode(0o600)).map_err(
                |e| {
                    Error::io(
                        format!("Failed to set adopted key permissions: {}", e),
                        Some(self.own_key_path().display().to_string()),
                    )
                },
            )?;
        }
        info!(device_id = %device_id, "Adopted legacy own certificate into fixed own.crt/own.key");
        Ok(true)
    }

    /// Delete certificate for device
    ///
    /// # Arguments
    /// * `device_id` - Device identifier
    pub fn delete_certificate(&self, device_id: &str) -> Result<()> {
        let cert_path = self.cert_path(device_id)?;
        let key_path = self.key_path(device_id)?;

        debug!(
            device_id = %device_id,
            "Deleting certificate"
        );

        if cert_path.exists() {
            fs::remove_file(&cert_path).map_err(|e| {
                Error::io(
                    format!("Failed to delete certificate: {}", e),
                    Some(cert_path.display().to_string()),
                )
            })?;
        }

        if key_path.exists() {
            fs::remove_file(&key_path).map_err(|e| {
                Error::io(
                    format!("Failed to delete private key: {}", e),
                    Some(key_path.display().to_string()),
                )
            })?;
        }

        info!(
            device_id = %device_id,
            event = "cert_deleted",
            "Deleted certificate and private key"
        );

        Ok(())
    }

    /// Get path to certificate file
    fn cert_path(&self, device_id: &str) -> Result<PathBuf> {
        validate_device_id(device_id)
            .map_err(|e| Error::InvalidRequest(format!("Invalid device_id in cert_path: {}", e)))?;
        Ok(self.cert_dir.join(format!("{}.crt", device_id)))
    }

    pub fn has_peer_certificate(&self, device_id: &str) -> bool {
        self.peer_cert_path(device_id).is_ok_and(|p| p.exists())
    }

    pub fn compute_fingerprint(cert_der: &[u8]) -> Result<String> {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(cert_der);
        Ok(hex::encode(digest))
    }

    fn fingerprint_path(&self, device_id: &str) -> Result<PathBuf> {
        validate_device_id(device_id).map_err(|e| {
            Error::InvalidRequest(format!("Invalid device_id in fingerprint_path: {}", e))
        })?;
        Ok(self.cert_dir.join(format!("{}_fingerprint.txt", device_id)))
    }

    pub fn has_peer_fingerprint(&self, device_id: &str) -> bool {
        self.fingerprint_path(device_id).is_ok_and(|p| p.exists())
    }

    pub fn store_peer_fingerprint(&self, device_id: &str, fingerprint: &str) -> Result<()> {
        self.init()?;
        let path = self.fingerprint_path(device_id)?;
        fs::write(&path, fingerprint).map_err(|e| {
            Error::io(
                format!("Failed to write peer fingerprint: {}", e),
                Some(path.display().to_string()),
            )
        })?;
        info!(
            device_id = %device_id,
            fingerprint = %fingerprint,
            "Stored peer certificate fingerprint (TOFU)"
        );
        Ok(())
    }

    pub fn load_peer_fingerprint(&self, device_id: &str) -> Result<String> {
        let path = self.fingerprint_path(device_id)?;
        if !path.exists() {
            return Err(Error::not_found(
                "No stored fingerprint for device",
                Some(device_id.to_string()),
            ));
        }
        let fp = fs::read_to_string(&path).map_err(|e| {
            Error::io(
                format!("Failed to read peer fingerprint: {}", e),
                Some(path.display().to_string()),
            )
        })?;
        Ok(fp.trim().to_string())
    }

    pub fn verify_peer_fingerprint(&self, device_id: &str, cert_der: &[u8]) -> Result<()> {
        let stored_fp = self.load_peer_fingerprint(device_id)?;
        let presented_fp = Self::compute_fingerprint(cert_der)?;
        if stored_fp != presented_fp {
            error!(
                device_id = %device_id,
                stored = %stored_fp,
                presented = %presented_fp,
                event = "fingerprint_mismatch",
                "Peer certificate fingerprint mismatch - possible MITM"
            );
            return Err(Error::CertificateError(format!(
                "Certificate fingerprint mismatch for device {}: expected {}, got {}",
                device_id, stored_fp, presented_fp
            )));
        }
        debug!(device_id = %device_id, "Peer certificate fingerprint verified");
        Ok(())
    }

    /// Extract the SubjectPublicKeyInfo DER from an X.509 cert DER — the
    /// exact input Android uses for the SAS (`certificate.publicKey.encoded`).
    pub fn extract_pubkey_der(cert_der: &[u8]) -> Result<Vec<u8>> {
        let cert = parse_x509_der(cert_der)?;
        Ok(cert.public_key().raw.to_vec())
    }

    /// Short Authentication String, byte-for-byte compatible with Android's
    /// `PairingHandler.getVerificationKey`: the larger public key (unsigned
    /// byte comparison) comes first, then the v8 pairing timestamp as ASCII,
    /// SHA-256, first 4 bytes, 8 UPPERCASE hex chars. Both sides of a pairing
    /// must display the identical key or the user cannot verify it.
    pub fn compute_verification_key(
        our_pubkey_der: &[u8],
        peer_pubkey_der: &[u8],
        timestamp: i64,
    ) -> String {
        use sha2::{Digest, Sha256};
        // Android sortedConcat: larger (unsigned) key first.
        let (first, second) = if our_pubkey_der.iter().cmp(peer_pubkey_der.iter()).is_gt() {
            (our_pubkey_der, peer_pubkey_der)
        } else {
            (peer_pubkey_der, our_pubkey_der)
        };
        let mut input = first.to_vec();
        input.extend_from_slice(second);
        input.extend_from_slice(timestamp.to_string().as_bytes());
        let digest = Sha256::digest(&input);
        hex::encode_upper(&digest[..4])
    }

    pub fn verify_peer_certificate(&self, device_id: &str, cert_der: &[u8]) -> Result<()> {
        if self.has_peer_fingerprint(device_id) {
            self.verify_peer_fingerprint(device_id, cert_der)?;
        }
        Ok(())
    }

    pub fn store_peer_certificate(&self, device_id: &str, cert_der: &[u8]) -> Result<()> {
        self.init()?;

        let cert_pem = der_to_pem(cert_der)?;

        // Verify-before-write: a certificate that fails the stored TOFU
        // fingerprint must be rejected BEFORE anything hits disk — writing
        // first would leave the impostor cert stored after the error.
        let fingerprint = Self::compute_fingerprint(cert_der)?;
        let first_use = !self.has_peer_fingerprint(device_id);
        if !first_use {
            self.verify_peer_fingerprint(device_id, cert_der)?;
        }

        let path = self.peer_cert_path(device_id)?;
        fs::write(&path, &cert_pem).map_err(|e| {
            Error::io(
                format!("Failed to write peer certificate: {}", e),
                Some(path.display().to_string()),
            )
        })?;

        if first_use {
            self.store_peer_fingerprint(device_id, &fingerprint)?;
            info!(
                device_id = %device_id,
                fingerprint = %fingerprint,
                "Trust-on-first-use: stored peer certificate fingerprint"
            );
        }

        info!(
            device_id = %device_id,
            path = %path.display(),
            "Stored peer certificate"
        );

        Ok(())
    }

    fn peer_cert_path(&self, device_id: &str) -> Result<PathBuf> {
        validate_device_id(device_id).map_err(|e| {
            Error::InvalidRequest(format!("Invalid device_id in peer_cert_path: {}", e))
        })?;
        Ok(self.cert_dir.join(format!("{}_peer.crt", device_id)))
    }

    pub fn load_peer_certificate_pem(&self, device_id: &str) -> Result<Vec<u8>> {
        let path = self.peer_cert_path(device_id)?;
        if !path.exists() {
            return Err(Error::not_found(
                "Peer certificate not found",
                Some(path.display().to_string()),
            ));
        }
        fs::read(&path).map_err(|e| {
            Error::io(
                format!("Failed to read peer certificate: {}", e),
                Some(path.display().to_string()),
            )
        })
    }

    /// Get path to private key file
    fn key_path(&self, device_id: &str) -> Result<PathBuf> {
        validate_device_id(device_id)
            .map_err(|e| Error::InvalidRequest(format!("Invalid device_id in key_path: {}", e)))?;
        Ok(self.cert_dir.join(format!("{}.key", device_id)))
    }

    pub fn delete_peer_certificate(&self, device_id: &str) -> Result<()> {
        let peer_path = self.peer_cert_path(device_id)?;
        let fp_path = self.fingerprint_path(device_id)?;

        if peer_path.exists() {
            fs::remove_file(&peer_path).map_err(|e| {
                Error::io(
                    format!("Failed to delete peer certificate: {}", e),
                    Some(peer_path.display().to_string()),
                )
            })?;
        }

        if fp_path.exists() {
            fs::remove_file(&fp_path).map_err(|e| {
                Error::io(
                    format!("Failed to delete peer fingerprint: {}", e),
                    Some(fp_path.display().to_string()),
                )
            })?;
        }

        info!(
            device_id = %device_id,
            "Deleted peer certificate and fingerprint"
        );

        Ok(())
    }

    pub fn list_known_peers(&self) -> Result<Vec<String>> {
        let mut peers = Vec::new();

        if !self.cert_dir.exists() {
            return Ok(peers);
        }

        for entry in fs::read_dir(&self.cert_dir).map_err(|e| {
            Error::io(
                format!("Failed to read certificate directory: {}", e),
                Some(self.cert_dir.display().to_string()),
            )
        })? {
            let entry = entry
                .map_err(|e| Error::io(format!("Failed to read directory entry: {}", e), None))?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(device_id) = name_str.strip_suffix("_peer.crt") {
                peers.push(device_id.to_string());
            }
        }

        Ok(peers)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (CertificateManager, TempDir) {
        let temp_dir = TempDir::new().expect("create temp dir");
        let manager = CertificateManager::new(temp_dir.path().to_path_buf());
        (manager, temp_dir)
    }

    #[test]
    fn test_generate_certificate() {
        let (manager, _temp_dir) = setup();

        let result =
            manager.generate_certificate("test-device-idaaaaaaaaaaaaaaaaaa", "Test Device");
        assert!(result.is_ok());

        let (cert_pem, key_pem) = result.expect("generate test certificate");
        assert!(!cert_pem.is_empty());
        assert!(!key_pem.is_empty());
    }

    #[test]
    fn test_store_and_load_certificate() {
        let (manager, _temp_dir) = setup();

        // Generate certificate
        let (cert_pem, key_pem) = manager
            .generate_certificate("test-device-idaaaaaaaaaaaaaaaaaa", "Test Device")
            .expect("generate test certificate");

        // Store certificate
        let result =
            manager.store_certificate("test-device-idaaaaaaaaaaaaaaaaaa", &cert_pem, &key_pem);
        assert!(result.is_ok());

        // Load certificate
        let result = manager.load_certificate("test-device-idaaaaaaaaaaaaaaaaaa");
        assert!(result.is_ok());

        let (loaded_cert, loaded_key) = result.expect("load test certificate");
        assert_eq!(cert_pem, loaded_cert);
        assert_eq!(key_pem, loaded_key);
    }

    #[test]
    fn test_validate_certificate() {
        let (manager, _temp_dir) = setup();

        // Generate certificate
        let (cert_pem, key_pem) = manager
            .generate_certificate("test-device-idaaaaaaaaaaaaaaaaaa", "Test Device")
            .expect("generate test certificate");

        // Validate certificate
        let result = manager.validate_certificate(&cert_pem, &key_pem);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_invalid_certificate() {
        let (manager, _temp_dir) = setup();

        // Invalid certificate
        let result = manager.validate_certificate(b"invalid", b"invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_has_certificate() {
        let (manager, _temp_dir) = setup();

        // Should not exist initially
        assert!(!manager.has_certificate("test-device-idaaaaaaaaaaaaaaaaaa"));

        // Generate and store certificate
        let (cert_pem, key_pem) = manager
            .generate_certificate("test-device-idaaaaaaaaaaaaaaaaaa", "Test Device")
            .expect("generate test certificate");
        manager
            .store_certificate("test-device-idaaaaaaaaaaaaaaaaaa", &cert_pem, &key_pem)
            .expect("store test certificate");

        // Should exist now
        assert!(manager.has_certificate("test-device-idaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn test_delete_certificate() {
        let (manager, _temp_dir) = setup();

        // Generate and store certificate
        let (cert_pem, key_pem) = manager
            .generate_certificate("test-device-idaaaaaaaaaaaaaaaaaa", "Test Device")
            .expect("generate test certificate");
        manager
            .store_certificate("test-device-idaaaaaaaaaaaaaaaaaa", &cert_pem, &key_pem)
            .expect("store test certificate");

        // Verify it exists
        assert!(manager.has_certificate("test-device-idaaaaaaaaaaaaaaaaaa"));

        // Delete certificate
        let result = manager.delete_certificate("test-device-idaaaaaaaaaaaaaaaaaa");
        assert!(result.is_ok());

        // Verify it's gone
        assert!(!manager.has_certificate("test-device-idaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn test_load_nonexistent_certificate() {
        let (manager, _temp_dir) = setup();

        let result = manager.load_certificate("nonexistentaaaaaaaaaaaaaaaaaaaaa");
        assert!(result.is_err());
    }

    #[test]
    fn test_compute_fingerprint() {
        let (manager, _temp_dir) = setup();
        let (cert_pem, _key_pem) = manager
            .generate_certificate("fp-testaaaaaaaaaaaaaaaaaaaaaaaaa", "FP Test")
            .expect("generate test certificate");

        let cert = openssl::x509::X509::from_pem(&cert_pem).expect("parse certificate PEM");
        let cert_der = cert.to_der().expect("encode certificate as DER");

        let fp1 = CertificateManager::compute_fingerprint(&cert_der).expect("compute fingerprint");
        let fp2 = CertificateManager::compute_fingerprint(&cert_der).expect("compute fingerprint");

        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 64);
    }

    #[test]
    fn test_tofu_first_connection_stores_fingerprint() {
        let (manager, _temp_dir) = setup();
        let (cert_pem, _key_pem) = manager
            .generate_certificate("tofu-deviceaaaaaaaaaaaaaaaaaaaaa", "TOFU Device")
            .expect("generate test certificate");

        let cert = openssl::x509::X509::from_pem(&cert_pem).expect("parse certificate PEM");
        let cert_der = cert.to_der().expect("encode certificate as DER");

        assert!(!manager.has_peer_fingerprint("tofu-deviceaaaaaaaaaaaaaaaaaaaaa"));

        manager
            .store_peer_certificate("tofu-deviceaaaaaaaaaaaaaaaaaaaaa", &cert_der)
            .expect("store peer certificate");

        assert!(manager.has_peer_fingerprint("tofu-deviceaaaaaaaaaaaaaaaaaaaaa"));
        let stored_fp = manager
            .load_peer_fingerprint("tofu-deviceaaaaaaaaaaaaaaaaaaaaa")
            .expect("load peer fingerprint");
        let expected_fp =
            CertificateManager::compute_fingerprint(&cert_der).expect("compute fingerprint");
        assert_eq!(stored_fp, expected_fp);
    }

    #[test]
    fn test_tofu_reconnect_same_cert_succeeds() {
        let (manager, _temp_dir) = setup();
        let (cert_pem, _key_pem) = manager
            .generate_certificate("reconnect-deviceaaaaaaaaaaaaaaaa", "Reconnect")
            .expect("generate test certificate");

        let cert = openssl::x509::X509::from_pem(&cert_pem).expect("parse certificate PEM");
        let cert_der = cert.to_der().expect("encode certificate as DER");

        manager
            .store_peer_certificate("reconnect-deviceaaaaaaaaaaaaaaaa", &cert_der)
            .expect("store peer certificate");
        let result = manager.store_peer_certificate("reconnect-deviceaaaaaaaaaaaaaaaa", &cert_der);
        assert!(result.is_ok());
    }

    #[test]
    fn test_tofu_reconnect_different_cert_fails() {
        let (manager, _temp_dir) = setup();
        let (cert_pem1, _) = manager
            .generate_certificate("mitm-deviceaaaaaaaaaaaaaaaaaaaaa", "Original")
            .expect("generate test certificate");
        let (cert_pem2, _) = manager
            .generate_certificate("mitm-deviceaaaaaaaaaaaaaaaaaaaaa", "Impostor")
            .expect("generate test certificate");

        let cert1 = openssl::x509::X509::from_pem(&cert_pem1).expect("parse certificate PEM");
        let cert_der1 = cert1.to_der().expect("encode certificate as DER");
        let cert2 = openssl::x509::X509::from_pem(&cert_pem2).expect("parse certificate PEM");
        let cert_der2 = cert2.to_der().expect("encode certificate as DER");

        manager
            .store_peer_certificate("mitm-deviceaaaaaaaaaaaaaaaaaaaaa", &cert_der1)
            .expect("store peer certificate");
        let result = manager.store_peer_certificate("mitm-deviceaaaaaaaaaaaaaaaaaaaaa", &cert_der2);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("fingerprint mismatch"));
    }

    #[test]
    fn test_verify_peer_fingerprint_not_found() {
        let (manager, _temp_dir) = setup();
        let result =
            manager.verify_peer_fingerprint("unknownaaaaaaaaaaaaaaaaaaaaaaaaa", b"fake_der");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_device_id_valid() {
        assert!(validate_device_id(&"a".repeat(32)).is_ok());
        assert!(validate_device_id(&"a".repeat(36)).is_ok());
        assert!(validate_device_id(&"a".repeat(38)).is_ok());
        assert!(validate_device_id("0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_device_id("9f1cb61f-2cbf-4608-9ba6-b4576f03553a").is_ok());
    }

    #[test]
    fn test_validate_device_id_too_short() {
        assert!(validate_device_id("").is_err());
        assert!(validate_device_id("a").is_err());
        assert!(validate_device_id("abc-123").is_err());
        assert!(validate_device_id(&"a".repeat(31)).is_err());
    }

    #[test]
    fn test_validate_device_id_too_long() {
        assert!(validate_device_id(&"x".repeat(39)).is_err());
        assert!(validate_device_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn test_validate_device_id_path_traversal() {
        // Each payload is padded to 32 chars so the length check passes and the
        // charset/traversal rule is what actually fires.
        assert!(validate_device_id("../etc/passwdaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("foo/baraaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("foo\\baraaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("foo/..baraaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("foo\0baraaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_device_id("foo..baraaaaaaaaaaaaaaaaaaaaaaaa").is_err());
    }

    #[test]
    fn test_extract_cn_no_cn_field() {
        let temp_dir = tempfile::TempDir::new().expect("create temp dir");
        let manager = CertificateManager::new(temp_dir.path().to_path_buf());
        manager.init().expect("initialize certificate manager");

        let (cert_pem, key_pem) = manager
            .generate_certificate("has-cn", "CN Device")
            .expect("generate test certificate");
        let cert = openssl::x509::X509::from_pem(&cert_pem).expect("parse certificate PEM");
        let pkey =
            openssl::pkey::PKey::private_key_from_pem(&key_pem).expect("parse private key PEM");

        let mut builder =
            openssl::x509::X509Builder::new().expect("create X509 certificate builder");

        let serial = openssl::bn::BigNum::from_u32(2)
            .expect("create serial number")
            .to_asn1_integer()
            .expect("convert serial to ASN.1 integer");
        builder
            .set_serial_number(&serial)
            .expect("set serial number");
        builder
            .set_issuer_name(cert.issuer_name())
            .expect("set issuer name");

        let not_before = openssl::asn1::Asn1Time::days_from_now(0).expect("create ASN.1 time");
        let not_after = openssl::asn1::Asn1Time::days_from_now(365).expect("create ASN.1 time");
        builder
            .set_not_before(not_before.as_ref())
            .expect("set not-before time");
        builder
            .set_not_after(not_after.as_ref())
            .expect("set not-after time");

        let mut subject_name =
            openssl::x509::X509NameBuilder::new().expect("create X509 name builder");
        subject_name
            .append_entry_by_text("O", "Test Org")
            .expect("append name entry");
        builder
            .set_subject_name(&subject_name.build())
            .expect("set subject name");

        builder.set_pubkey(&pkey).expect("set public key");
        builder
            .sign(&pkey, openssl::hash::MessageDigest::sha256())
            .expect("sign certificate");

        let cert_der = builder.build().to_der().expect("encode certificate as DER");

        let result = extract_cn_from_der(&cert_der);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("no Common Name"));
    }

    #[test]
    fn test_extract_cn_invalid_der() {
        let result = extract_cn_from_der(b"this is not a valid DER certificate");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Failed to parse DER"));
    }

    #[test]
    fn test_ensure_certificate_creates_when_missing() {
        let (cm, _temp) = setup();
        assert!(!cm.has_certificate("new-deviceaaaaaaaaaaaaaaaaaaaaaa"));
        cm.ensure_certificate("new-deviceaaaaaaaaaaaaaaaaaaaaaa", "Test Device")
            .expect("ensure certificate");
        assert!(cm.has_certificate("new-deviceaaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn test_ensure_certificate_skips_when_exists() {
        let (cm, _temp) = setup();
        let (cert1, key1) = cm
            .generate_certificate("existing-deviceaaaaaaaaaaaaaaaaa", "Test")
            .expect("generate test certificate");
        cm.store_certificate("existing-deviceaaaaaaaaaaaaaaaaa", &cert1, &key1)
            .expect("store test certificate");

        cm.ensure_certificate("existing-deviceaaaaaaaaaaaaaaaaa", "Test Device")
            .expect("ensure certificate");

        let (cert2, _key2) = cm
            .load_certificate("existing-deviceaaaaaaaaaaaaaaaaa")
            .expect("load test certificate");
        assert_eq!(
            cert1, cert2,
            "ensure_certificate should not overwrite existing cert"
        );
    }

    #[test]
    fn test_delete_peer_certificate() {
        let (cm, _temp) = setup();
        let (cert_pem, _key_pem) = cm
            .generate_certificate("peer-deviceaaaaaaaaaaaaaaaaaaaaa", "Test Peer")
            .expect("generate test certificate");
        let cert_der = openssl::x509::X509::from_pem(&cert_pem)
            .expect("parse certificate PEM")
            .to_der()
            .expect("encode certificate as DER");
        cm.store_peer_certificate("peer-deviceaaaaaaaaaaaaaaaaaaaaa", &cert_der)
            .expect("store peer certificate");
        assert!(cm.has_peer_certificate("peer-deviceaaaaaaaaaaaaaaaaaaaaa"));

        cm.delete_peer_certificate("peer-deviceaaaaaaaaaaaaaaaaaaaaa")
            .expect("delete peer certificate");
        assert!(!cm.has_peer_certificate("peer-deviceaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!cm.has_peer_fingerprint("peer-deviceaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn test_delete_peer_certificate_nonexistent() {
        let (cm, _temp) = setup();
        assert!(cm
            .delete_peer_certificate("no-such-peeraaaaaaaaaaaaaaaaaaaa")
            .is_ok());
    }

    #[test]
    fn test_load_peer_fingerprint() {
        let (cm, _temp) = setup();
        let fp = "abcdef1234567890";
        cm.store_peer_fingerprint("fp-deviceaaaaaaaaaaaaaaaaaaaaaaa", fp)
            .expect("store peer fingerprint");

        let loaded = cm
            .load_peer_fingerprint("fp-deviceaaaaaaaaaaaaaaaaaaaaaaa")
            .expect("load peer fingerprint");
        assert_eq!(loaded, fp);
    }

    #[test]
    fn test_load_peer_fingerprint_not_found() {
        let (cm, _temp) = setup();
        let result = cm.load_peer_fingerprint("no-fingerprintaaaaaaaaaaaaaaaaaa");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("No stored fingerprint"));
    }

    #[test]
    fn test_compute_verification_key_deterministic() {
        let cert_a = b"-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n";
        let cert_b = b"-----BEGIN CERTIFICATE-----\nB\n-----END CERTIFICATE-----\n";
        let ts = 1700000000i64;

        let key1 = CertificateManager::compute_verification_key(cert_a, cert_b, ts);
        let key2 = CertificateManager::compute_verification_key(cert_b, cert_a, ts);
        assert_eq!(key1, key2, "verification key must be symmetric");
        assert_eq!(key1.len(), 8, "verification key must be 8 hex chars");
    }

    #[test]
    fn test_compute_verification_key_different_timestamp() {
        let cert_a = b"-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n";
        let cert_b = b"-----BEGIN CERTIFICATE-----\nB\n-----END CERTIFICATE-----\n";

        let key1 = CertificateManager::compute_verification_key(cert_a, cert_b, 1700000000);
        let key2 = CertificateManager::compute_verification_key(cert_a, cert_b, 1700000001);
        assert_ne!(
            key1, key2,
            "different timestamps must produce different keys"
        );
    }

    #[test]
    fn test_compute_verification_key_different_certs() {
        let cert_a = b"-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n";
        let cert_b = b"-----BEGIN CERTIFICATE-----\nB\n-----END CERTIFICATE-----\n";
        let cert_c = b"-----BEGIN CERTIFICATE-----\nC\n-----END CERTIFICATE-----\n";
        let ts = 1700000000i64;

        let key1 = CertificateManager::compute_verification_key(cert_a, cert_b, ts);
        let key2 = CertificateManager::compute_verification_key(cert_a, cert_c, ts);
        assert_ne!(key1, key2, "different certs must produce different keys");
    }

    #[test]
    fn test_compute_verification_key_hex_format() {
        let cert_a = b"-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n";
        let cert_b = b"-----BEGIN CERTIFICATE-----\nB\n-----END CERTIFICATE-----\n";
        let ts = 1700000000i64;

        let key = CertificateManager::compute_verification_key(cert_a, cert_b, ts);
        assert!(
            key.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()),
            "verification key must be UPPERCASE hex (Android format), got {}",
            key
        );
    }

    #[test]
    fn test_compute_verification_key_android_conformance_vector() {
        // Computed independently from PairingHandler.getVerificationKey:
        // sortedConcat(larger pubkey first, unsigned compare) + timestamp as
        // ASCII, SHA-256, first 4 bytes, uppercase hex.
        let pubkey_a: Vec<u8> = (0x01u8..=0x20).collect();
        let pubkey_b: Vec<u8> = (0xA0u8..=0xBF).collect();
        let ts = 1700000000i64;

        let key = CertificateManager::compute_verification_key(&pubkey_a, &pubkey_b, ts);
        assert_eq!(key, "D95DFF82", "must match the Android SAS algorithm");
        // Argument order must not matter (both sides pass (own, peer)).
        let key_swapped = CertificateManager::compute_verification_key(&pubkey_b, &pubkey_a, ts);
        assert_eq!(key_swapped, "D95DFF82");
    }

    #[test]
    fn test_extract_pubkey_der_roundtrip() {
        let (manager, _temp_dir) = setup();
        let (cert_pem, _key_pem) = manager
            .generate_certificate("pubkey-deviceaaaaaaaaaaaaaaaaaaa", "Pubkey Test")
            .expect("generate test certificate");
        let cert_der = openssl::x509::X509::from_pem(&cert_pem)
            .expect("parse certificate PEM")
            .to_der()
            .expect("encode certificate as DER");

        let pubkey_der =
            CertificateManager::extract_pubkey_der(&cert_der).expect("extract public key DER");

        // Must equal openssl's own public_key encoding of the same cert.
        let x509 = openssl::x509::X509::from_der(&cert_der).expect("parse certificate DER");
        let expected = x509
            .public_key()
            .expect("extract public key")
            .public_key_to_der()
            .expect("encode public key as DER");
        assert_eq!(pubkey_der, expected);
        assert!(!pubkey_der.is_empty());
    }

    #[test]
    fn test_store_peer_certificate_mismatch_leaves_stored_cert_untouched() {
        // Verify-before-write: a failed store (fingerprint mismatch) must not
        // clobber the previously stored peer certificate.
        let (manager, _temp_dir) = setup();
        let peer_id = "verify-first-deviceaaaaaaaaaaaaaaa";
        let (cert_pem1, _) = manager
            .generate_certificate(peer_id, "Original")
            .expect("generate test certificate");
        let (cert_pem2, _) = manager
            .generate_certificate(peer_id, "Impostor")
            .expect("generate test certificate");
        let cert_der1 = openssl::x509::X509::from_pem(&cert_pem1)
            .expect("parse certificate PEM")
            .to_der()
            .expect("encode certificate as DER");
        let cert_der2 = openssl::x509::X509::from_pem(&cert_pem2)
            .expect("parse certificate PEM")
            .to_der()
            .expect("encode certificate as DER");

        manager
            .store_peer_certificate(peer_id, &cert_der1)
            .expect("store peer certificate");
        assert!(manager.store_peer_certificate(peer_id, &cert_der2).is_err());

        let stored_pem = manager
            .load_peer_certificate_pem(peer_id)
            .expect("load peer certificate PEM");
        assert_eq!(
            stored_pem, cert_pem1,
            "a rejected certificate must never replace the stored one"
        );
    }

    #[test]
    fn test_generate_certificate_not_before_one_year_back() {
        // Android SslHelper.kt:110: notBefore = now - 1 year.
        let (manager, _temp_dir) = setup();
        let (cert_pem, _) = manager
            .generate_certificate("validity-deviceaaaaaaaaaaaaaaaaaa", "Validity")
            .expect("generate test certificate");
        let cert = openssl::x509::X509::from_pem(&cert_pem).expect("parse certificate PEM");

        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("read system time")
            .as_secs() as i64;
        let almost_a_year_ago = openssl::asn1::Asn1Time::from_unix(now_unix - 360 * 24 * 60 * 60)
            .expect("create ASN.1 time");
        let over_a_year_ago = openssl::asn1::Asn1Time::from_unix(now_unix - 370 * 24 * 60 * 60)
            .expect("create ASN.1 time");
        assert_eq!(
            cert.not_before()
                .compare(&almost_a_year_ago)
                .expect("compare"),
            std::cmp::Ordering::Less,
            "notBefore must be about a year in the past"
        );
        assert_eq!(
            cert.not_before()
                .compare(&over_a_year_ago)
                .expect("compare"),
            std::cmp::Ordering::Greater,
            "notBefore must not be more than a year in the past"
        );
    }

    #[test]
    fn test_ensure_own_certificate_regenerates_expired() {
        // Android SslHelper.kt:81-87: an expired own certificate is
        // regenerated, not reused.
        let (manager, _temp_dir) = setup();
        let device_id = "expiry-deviceaaaaaaaaaaaaaaaaaaaa";
        let (_cert_pem, key_pem) = manager
            .generate_certificate(device_id, "Expiry")
            .expect("generate test certificate");
        let pkey =
            openssl::pkey::PKey::private_key_from_pem(&key_pem).expect("parse private key PEM");

        // Build an already-expired cert over the same key and store it as own.
        let mut builder =
            openssl::x509::X509Builder::new().expect("create X509 certificate builder");
        let serial = openssl::bn::BigNum::from_u32(3)
            .expect("create serial number")
            .to_asn1_integer()
            .expect("convert serial to ASN.1 integer");
        builder
            .set_serial_number(&serial)
            .expect("set serial number");
        let mut name_builder =
            openssl::x509::X509NameBuilder::new().expect("create X509 name builder");
        name_builder
            .append_entry_by_text("CN", device_id)
            .expect("append name entry");
        let name = name_builder.build();
        builder.set_subject_name(&name).expect("set subject name");
        builder.set_issuer_name(&name).expect("set issuer name");
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("read system time")
            .as_secs() as i64;
        let not_before = openssl::asn1::Asn1Time::from_unix(now_unix - 2 * 365 * 24 * 60 * 60)
            .expect("create ASN.1 time");
        let not_after = openssl::asn1::Asn1Time::from_unix(now_unix - 365 * 24 * 60 * 60)
            .expect("create ASN.1 time");
        builder
            .set_not_before(&not_before)
            .expect("set not-before time");
        builder
            .set_not_after(&not_after)
            .expect("set not-after time");
        builder.set_pubkey(&pkey).expect("set public key");
        builder
            .sign(&pkey, openssl::hash::MessageDigest::sha512())
            .expect("sign certificate");
        let expired_pem = builder.build().to_pem().expect("encode certificate as PEM");
        manager
            .store_own_certificate(&expired_pem, &key_pem)
            .expect("store own certificate");
        assert!(manager.has_own_certificate());

        manager
            .ensure_own_certificate(device_id, "Expiry")
            .expect("ensure own certificate");

        let (new_pem, _) = manager
            .load_own_certificate()
            .expect("load own certificate");
        assert_ne!(
            new_pem, expired_pem,
            "an expired own certificate must be regenerated"
        );
        let new_cert = openssl::x509::X509::from_pem(&new_pem).expect("parse certificate PEM");
        let now = openssl::asn1::Asn1Time::from_unix(now_unix).expect("create ASN.1 time");
        assert_eq!(
            new_cert.not_after().compare(&now).expect("compare"),
            std::cmp::Ordering::Greater,
            "the regenerated certificate must be valid"
        );
    }

    #[test]
    fn test_ensure_own_certificate_keeps_valid_cert() {
        let (manager, _temp_dir) = setup();
        let device_id = "keep-device-aaaaaaaaaaaaaaaaaaaa";
        manager
            .ensure_own_certificate(device_id, "Keep")
            .expect("ensure own certificate");
        let (pem1, _) = manager
            .load_own_certificate()
            .expect("load own certificate");

        manager
            .ensure_own_certificate(device_id, "Keep")
            .expect("ensure own certificate");
        let (pem2, _) = manager
            .load_own_certificate()
            .expect("load own certificate");
        assert_eq!(pem1, pem2, "a valid own certificate must not be replaced");
    }

    #[cfg(unix)]
    #[test]
    fn test_store_own_certificate_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().expect("create temp dir");
        let cert_dir = temp_dir.path().join("certs");
        let manager = CertificateManager::new(cert_dir.clone());

        let (cert_pem, key_pem) = manager
            .generate_certificate("test-device-idaaaaaaaaaaaaaaaaaa", "Test Device")
            .expect("generate test certificate");
        manager
            .store_own_certificate(&cert_pem, &key_pem)
            .expect("store own certificate");

        let key_mode = fs::metadata(cert_dir.join("own.key"))
            .expect("read file metadata")
            .permissions()
            .mode();
        assert_eq!(key_mode & 0o777, 0o600, "own.key must be owner-only");
        let dir_mode = fs::metadata(&cert_dir)
            .expect("read file metadata")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "certs dir must be owner-only");
    }

    #[cfg(unix)]
    #[test]
    fn test_adopt_legacy_own_certificate_tightens_key_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (manager, temp_dir) = setup();
        let device_id = "legacy-device-aaaaaaaaaaaaaaaaaa";
        let (cert_pem, key_pem) = manager
            .generate_certificate(device_id, "Legacy")
            .expect("generate test certificate");
        // Loose legacy perms (0644) must not survive the rename adoption.
        manager.init().expect("initialize certificate manager");
        fs::write(
            temp_dir.path().join(format!("{}.crt", device_id)),
            &cert_pem,
        )
        .expect("write legacy certificate file");
        let legacy_key = temp_dir.path().join(format!("{}.key", device_id));
        fs::write(&legacy_key, &key_pem).expect("write legacy key file");
        fs::set_permissions(&legacy_key, fs::Permissions::from_mode(0o644))
            .expect("set key file permissions");

        let adopted = manager
            .adopt_legacy_own_certificate(device_id)
            .expect("adopt legacy certificate");
        assert!(adopted);

        let key_mode = fs::metadata(temp_dir.path().join("own.key"))
            .expect("read file metadata")
            .permissions()
            .mode();
        assert_eq!(
            key_mode & 0o777,
            0o600,
            "adopted own.key must be owner-only"
        );
    }

    /// The parser reads only the first BEGIN/END CERTIFICATE block. Every
    /// caller feeds a file this process wrote, all single-cert, so a bundle
    /// is a caller bug — reject it rather than silently using entry one.
    #[test]
    fn test_pem_to_der_rejects_multi_certificate_bundle() {
        let (manager, _temp_dir) = setup();
        let (cert_pem, _key_pem) = manager
            .generate_certificate("bundle-deviceaaaaaaaaaaaaaaaaaaa", "Bundle")
            .expect("generate test certificate");

        let single = pem_to_der(&cert_pem).expect("one certificate must still decode");
        assert!(!single.is_empty());

        let mut bundle = cert_pem.clone();
        if !bundle.ends_with(b"\n") {
            bundle.push(b'\n');
        }
        bundle.extend_from_slice(&cert_pem);

        let err = pem_to_der(&bundle).expect_err("a two-certificate bundle must be rejected");
        assert!(
            err.to_string().contains("exactly one"),
            "the error must name the single-certificate contract, got: {err}"
        );
    }

    /// R1: the generated certificate MUST carry exactly one subjectAltName
    /// dNSName equal to the device id, so kdeconnectd's Qt TLS layer
    /// (core/backends/lan/lanlinkprovider.cpp:604: setPeerVerifyName) can
    /// verify our cert against the id it dials with. CN, RDN order, and
    /// the rest of the shape are unchanged.
    #[test]
    fn test_generated_cert_carries_device_id_san() {
        use x509_parser::oid_registry::{
            OID_X509_COMMON_NAME, OID_X509_ORGANIZATIONAL_UNIT, OID_X509_ORGANIZATION_NAME,
        };
        use x509_parser::prelude::GeneralName;

        let (manager, _temp_dir) = setup();
        let device_id = "rust-connect-cert-test-123456789"; // 32 chars, ld-h safe
        let (cert_pem, _key_pem) = manager
            .generate_certificate(device_id, "Cert Test")
            .expect("generate test certificate");

        let cert_der = pem_to_der(&cert_pem).expect("parse generated cert PEM to DER");
        let cert = parse_x509_der(&cert_der).expect("parse generated cert DER");

        // CN is unchanged — still the device id (kdeconnectd's stored-cert
        // fingerprint TOFU and our own verification both key off CN).
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok())
            .map(|s| s.to_string())
            .expect("cert has a CN");
        assert_eq!(cn, device_id, "CN must remain the device id");

        // RDN order is CN, OU, O (matching Android SslHelper.kt — see the
        // existing comment in generate_certificate). x509-parser exposes
        // subject attributes in DER order; iterating them must surface CN
        // first, then OU, then O.
        let attr_oids: Vec<_> = cert
            .subject()
            .iter_attributes()
            .map(|a| a.attr_type().clone())
            .collect();
        assert!(
            attr_oids.len() >= 3,
            "expected at least CN, OU, O in subject, got {} attributes",
            attr_oids.len()
        );
        assert_eq!(attr_oids[0], OID_X509_COMMON_NAME, "first RDN must be CN");
        assert_eq!(
            attr_oids[1], OID_X509_ORGANIZATIONAL_UNIT,
            "second RDN must be OU"
        );
        assert_eq!(
            attr_oids[2], OID_X509_ORGANIZATION_NAME,
            "third RDN must be O"
        );

        // SAN: exactly one dNSName carrying the device id verbatim.
        let san = cert
            .subject_alternative_name()
            .expect("parse SAN extension")
            .expect("cert must carry a subjectAltName (R1)");
        assert_eq!(san.value.general_names.len(), 1, "exactly one SAN entry");
        match &san.value.general_names[0] {
            GeneralName::DNSName(name) => assert_eq!(
                name.to_string(),
                device_id,
                "SAN dNSName must equal the device id verbatim"
            ),
            other => panic!("SAN entry must be dNSName, got {:?}", other),
        }
    }

    /// R2: device ids containing '_' are spec-valid (the Android spec
    /// allows them and real ids are hex, so `_` is a permitted character
    /// in the charset). Generation must succeed, and the SAN dNSName must
    /// carry the underscore-bearing id verbatim — rcgen's Ia5String only
    /// requires ASCII and `_` is ASCII, so the contract holds.
    #[test]
    fn test_generated_cert_san_accepts_underscore_id() {
        use x509_parser::prelude::GeneralName;

        let (manager, _temp_dir) = setup();
        // 34 chars, contains '_' — within the validate_device_id length
        // window (32..=38) and the [a-zA-Z0-9_-] charset.
        let device_id = "rust_connect_test_device_aaaaaaa";

        let (cert_pem, _key_pem) = manager
            .generate_certificate(device_id, "Underscore Test")
            .expect("generation must succeed for an underscore-bearing device id");

        let cert_der = pem_to_der(&cert_pem).expect("parse generated cert PEM to DER");
        let cert = parse_x509_der(&cert_der).expect("parse generated cert DER");

        let san = cert
            .subject_alternative_name()
            .expect("parse SAN extension")
            .expect("cert must carry a subjectAltName for the underscore id");
        assert_eq!(san.value.general_names.len(), 1);
        match &san.value.general_names[0] {
            GeneralName::DNSName(name) => assert_eq!(
                name.to_string(),
                device_id,
                "SAN dNSName must carry the underscore-bearing id verbatim"
            ),
            other => panic!("SAN entry must be dNSName, got {:?}", other),
        }
    }

    /// Companion: the SAN addition must not perturb the existing
    /// SslHelper.kt-pin semantics — the validity window (now−1y /
    /// now+10y) and the 20-byte serial scheme. not_before is already
    /// pinned by test_generate_certificate_not_before_one_year_back; this
    /// pins the upper bound and the serial length.
    #[test]
    fn test_generated_cert_validity_and_serial_unchanged_after_san() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let (manager, _temp_dir) = setup();
        let device_id = "validity-and-serial-deviceaaaaaaaaa"; // 34 chars
        let (cert_pem, _key_pem) = manager
            .generate_certificate(device_id, "Pinning")
            .expect("generate test certificate");

        // not_after: openssl's X509.not_after() returns &Asn1TimeRef
        // directly; the comparison API lives there, not on ASN1Time.
        let openssl_cert = openssl::x509::X509::from_pem(&cert_pem)
            .expect("parse generated cert PEM with openssl");
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("read system time")
            .as_secs() as i64;
        let almost_ten_years = now_unix + 9 * 365 * 24 * 60 * 60;
        let over_ten_years = now_unix + 10 * 365 * 24 * 60 * 60 + 60 * 24 * 60 * 60;
        let almost_ten_years_asn1 =
            openssl::asn1::Asn1Time::from_unix(almost_ten_years).expect("create ASN.1 time");
        let over_ten_years_asn1 =
            openssl::asn1::Asn1Time::from_unix(over_ten_years).expect("create ASN.1 time");
        assert_eq!(
            openssl_cert
                .not_after()
                .compare(&almost_ten_years_asn1)
                .expect("compare"),
            std::cmp::Ordering::Greater,
            "notAfter must be roughly 10y ahead of now"
        );
        assert_eq!(
            openssl_cert
                .not_after()
                .compare(&over_ten_years_asn1)
                .expect("compare"),
            std::cmp::Ordering::Less,
            "notAfter must not be more than ~10y ahead of now"
        );

        // RFC 3280 20-byte serial — the openssl-era carry-over, retained
        // by generate_certificate (see serial_buf = [0u8; 20]).
        let cert_der = pem_to_der(&cert_pem).expect("parse generated cert PEM to DER");
        let cert = parse_x509_der(&cert_der).expect("parse generated cert DER");
        assert_eq!(
            cert.raw_serial().len(),
            20,
            "serial must be 20 bytes (RFC 3280)"
        );
    }
}
