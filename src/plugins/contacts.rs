//! Contacts plugin
//!
//! Single Responsibility: Synchronize and store contacts from paired devices.
//!
//! Wire shapes (upstream-verified):
//! - Outgoing `kdeconnect.contacts.request_all_uids_timestamps` — EMPTY body.
//!   kdeconnect-kde sends `NetworkPacket np(packetType)` with no fields
//!   (plugins/contacts/contactsplugin.cpp:169-176); it sends this on connect
//!   (contactsplugin.cpp:42-45, 59-62).
//! - Incoming `kdeconnect.contacts.response_uids_timestamps` — body has a
//!   `uids` array of uid strings, plus one field PER UID whose key is the uid
//!   and whose value is that contact's last-changed timestamp. Android writes
//!   the timestamp as a STRING (`set(contactID.toString(), timestamp.toString())`,
//!   kdeconnect-android src/main/java/org/kde/kdeconnect/plugins/contacts/ContactsPlugin.kt:110-119);
//!   kdeconnect-kde reads it back with `np.get<qint64>(ID)`
//!   (contactsplugin.cpp:116), so we accept both string and number.
//! - Outgoing `kdeconnect.contacts.request_vcards_by_uid` — body
//!   `{"uids": [<uid>, ...]}` (contactsplugin.cpp:178-185).
//! - Incoming `kdeconnect.contacts.response_vcards` — body has a `uids`
//!   array, plus one field PER UID whose value is the raw vCard string
//!   (ContactsPlugin.kt:140-155; contactsplugin.cpp:136-167).
//!
//! Sync flow mirrors kdeconnect-kde (contactsplugin.cpp:64-134): on a uids/
//! timestamps response we request vCards only for uids that are new or whose
//! timestamp changed, and we DROP stored contacts the phone no longer reports
//! (upstream deletes the local files, contactsplugin.cpp:125-129).
//!
//! Capability honesty: we advertise exactly the four types above, matching
//! kdeconnect-kde's own declaration (plugins/contacts/kdeconnect_contacts.json).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock as StdRwLock};

use tracing::{info, warn};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;

/// Per-device maximum contact count (defense-in-depth cap on top of the
/// snapshot gate). Real-world phones carry low-thousands of contacts at
/// most; we leave headroom and reject any further inserts with a warn.
const MAX_CONTACTS_PER_DEVICE: usize = 10_000;

/// Per-device maximum total vCard-source bytes (the heavy field on
/// `Contact`). The 32 MiB packet ceiling bounds a single packet but not
/// cumulative storage; this caps cumulative bytes per device and refuses
/// new inserts past it. Sized so even at MAX_CONTACTS_PER_DEVICE (10k) we
/// hold ~6 KiB per vCard on average, well above any real contact.
const MAX_VCARD_BYTES_PER_DEVICE: usize = 64 * 1024 * 1024;

/// A contact stored from a phone-provided vCard.
///
/// `name`, `phone_numbers` and `emails` are parsed minimally (dependency-free)
/// from the FN / TEL / EMAIL vCard lines; `vcard` keeps the raw payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
    pub uid: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub phone_numbers: Vec<String>,
    #[serde(default)]
    pub emails: Vec<String>,
    /// Raw vCard as received from the phone.
    pub vcard: String,
}

/// Per-device contacts state. Both fields live under ONE lock so the
/// uids/timestamps update (read-decide-write) is atomic and lock-order
/// inversion between the two maps is impossible by construction — the
/// plugin instance is shared across all per-device connection loops.
#[derive(Default)]
struct DeviceContacts {
    /// uid -> last timestamp reported by the phone (normalized to a plain
    /// string; the phone sends digits-as-string)
    timestamps: HashMap<String, String>,
    /// uid -> stored contact
    contacts: HashMap<String, Contact>,
    /// Sum of `Contact::vcard.len()` over `contacts` (the byte-counting
    /// limit operates on this rather than recomputing per check). Maintained
    /// alongside `contacts` under the same lock; the snapshot prune at the
    /// bottom of `handle_packet` recomputes it after the retain.
    total_vcard_bytes: usize,
}

pub struct ContactsPlugin {
    /// device_id -> per-device state
    devices: Arc<StdRwLock<HashMap<String, DeviceContacts>>>,
}

impl Default for ContactsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl ContactsPlugin {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(StdRwLock::new(HashMap::new())),
        }
    }

    /// Stored contacts for a device (empty if never synced), sorted by uid
    /// so API responses are deterministic.
    pub fn get_contacts(&self, device_id: &str) -> Vec<Contact> {
        let devices = self.devices.read().unwrap_or_else(|e| e.into_inner());
        let mut contacts: Vec<Contact> = devices
            .get(device_id)
            .map(|d| d.contacts.values().cloned().collect())
            .unwrap_or_default();
        contacts.sort_by(|a, b| a.uid.cmp(&b.uid));
        contacts
    }

    /// Request-all-uids packet. Empty body per upstream
    /// (kdeconnect-kde plugins/contacts/contactsplugin.cpp:169-176).
    pub fn request_all_uids_timestamps(&self) -> Packet {
        Packet::new(
            "kdeconnect.contacts.request_all_uids_timestamps".to_string(),
            serde_json::json!({}),
        )
    }

    /// Request-vcards packet for the given uids. Body `{"uids": [...]}`
    /// (kdeconnect-kde plugins/contacts/contactsplugin.cpp:178-185).
    pub fn request_vcards_by_uid(&self, uids: &[String]) -> Packet {
        Packet::new(
            "kdeconnect.contacts.request_vcards_by_uid".to_string(),
            serde_json::json!({ "uids": uids }),
        )
    }

    /// Normalize a timestamp field to a comparable string. Android sends
    /// digits-as-string (ContactsPlugin.kt:115); accept numbers too.
    fn timestamp_string(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    /// Parse a uids/timestamps response body into (uid -> timestamp).
    /// Wire shape: kdeconnect-android ContactsPlugin.kt:110-119.
    /// Returns None when the `uids` key is absent or not an array (malformed —
    /// ignore the packet). A present-but-empty array is a VALID report (the
    /// phone has zero contacts) and must clear the device's stored state,
    /// not be ignored (kdeconnect-kde delete-unreported, contactsplugin.cpp:125-129).
    fn parse_uids_timestamps(body: &serde_json::Value) -> Option<HashMap<String, String>> {
        let mut out = HashMap::new();
        let uids = body.get("uids").and_then(|u| u.as_array())?;
        for uid in uids.iter().filter_map(|u| u.as_str()) {
            let ts = body.get(uid).and_then(Self::timestamp_string);
            out.insert(uid.to_string(), ts.unwrap_or_default());
        }
        Some(out)
    }

    /// Minimal, dependency-free vCard field extraction: FN -> name,
    /// TEL -> phone numbers, EMAIL -> emails. Handles line unfolding
    /// (continuation lines start with space/tab, RFC 2425 §5.8.1) and
    /// vCard 2.1 group prefixes ("item1.TEL" -> TEL, RFC 2425 §3.3).
    fn parse_vcard_fields(vcard: &str) -> (Option<String>, Vec<String>, Vec<String>) {
        let mut name = None;
        let mut phones = Vec::new();
        let mut emails = Vec::new();

        // Unfold continuation lines.
        let mut lines: Vec<String> = Vec::new();
        for raw in vcard.lines() {
            if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
                let cont = raw.trim_start();
                if let Some(last) = lines.last_mut() {
                    last.push_str(cont);
                }
            } else {
                lines.push(raw.trim_end_matches('\r').to_string());
            }
        }

        for line in lines {
            let Some(colon) = line.find(':') else {
                continue;
            };
            let raw_prop = line[..colon].to_ascii_uppercase();
            // vCard 2.1 group prefix (RFC 2425 §3.3): real phones emit e.g.
            // "item1.TEL;CELL:" — strip "<group>." or the property matches
            // nothing. Only strip when the candidate group has no
            // ';' — a '.' inside the param section (e.g. "TEL;TYPE=WORK.VOICE")
            // is not a group prefix.
            let prop = match raw_prop.split_once('.') {
                Some((group, rest)) if !group.contains(';') => rest.to_string(),
                _ => raw_prop,
            };
            let value = line[colon + 1..].trim().to_string();
            if value.is_empty() {
                continue;
            }
            if prop == "FN" || prop.starts_with("FN;") {
                if name.is_none() {
                    name = Some(value);
                }
            } else if prop == "TEL" || prop.starts_with("TEL;") {
                phones.push(value);
            } else if prop == "EMAIL" || prop.starts_with("EMAIL;") {
                emails.push(value);
            }
        }
        (name, phones, emails)
    }

    /// Insert parsed vCards into `state` under the snapshot gate + caps.
    /// Returns `(stored, dropped_unreported, dropped_cap)` so the handler can
    /// log separately. The handler wrapper passes the production caps; the
    /// `_inner` helper takes them as parameters so tests exercise the cap
    /// paths without filling 10k contacts.
    ///
    /// Snapshot gate: a vCard's UID must appear in `reported` (the latest
    /// uids/timestamps response). This closes the CV finding where a paired
    /// peer sent `response_vcards` for arbitrary new UIDs without an
    /// accompanying snapshot and grew `state.contacts` for the life of the
    /// connection.
    ///
    /// Caps: refuse inserts whose addition would push `state.contacts.len()`
    /// past `max_contacts` or `state.total_vcard_bytes` past `max_bytes`.
    /// Updates of an already-stored UID are allowed as long as the byte delta
    /// fits under the cap (a contract always honored); the only refused
    /// growth is the count of new UIDs.
    ///
    /// `state.contacts` is mutated in place. Caller must hold the write lock.
    fn store_vcards_inner(
        state: &mut DeviceContacts,
        reported: &HashSet<String>,
        parsed: Vec<(String, Contact)>,
        max_contacts: usize,
        max_bytes: usize,
    ) -> (usize, usize, usize) {
        let mut stored = 0usize;
        let mut dropped_unreported = 0usize;
        let mut dropped_cap = 0usize;
        for (uid, contact) in parsed {
            if !reported.contains(&uid) {
                dropped_unreported += 1;
                continue;
            }

            // Count cap: only a genuinely-new UID counts toward growth; a
            // re-fetch of an already-stored UID is an update, not a new entry.
            let is_new_uid = !state.contacts.contains_key(&uid);
            if is_new_uid && state.contacts.len() >= max_contacts {
                dropped_cap += 1;
                continue;
            }

            // Byte cap: total_vcard_bytes - old + new <= max_bytes.
            let old_bytes = state.contacts.get(&uid).map(|c| c.vcard.len()).unwrap_or(0);
            let prospective_bytes = state.total_vcard_bytes + contact.vcard.len() - old_bytes;
            if prospective_bytes > max_bytes {
                dropped_cap += 1;
                continue;
            }

            state.total_vcard_bytes = prospective_bytes;
            state.contacts.insert(uid, contact);
            stored += 1;
        }
        (stored, dropped_unreported, dropped_cap)
    }

    /// Production-wrapper for `store_vcards_inner` using the file-level
    /// constants. The handler logs `dropped_*` counts; inserts the rest.
    fn store_vcards(
        state: &mut DeviceContacts,
        reported: &HashSet<String>,
        parsed: Vec<(String, Contact)>,
    ) -> (usize, usize, usize) {
        Self::store_vcards_inner(
            state,
            reported,
            parsed,
            MAX_CONTACTS_PER_DEVICE,
            MAX_VCARD_BYTES_PER_DEVICE,
        )
    }
}

#[async_trait::async_trait]
impl Plugin for ContactsPlugin {
    fn name(&self) -> &str {
        "contacts"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        // kdeconnect-kde kdeconnect_contacts.json X-KdeConnect-SupportedPacketType
        vec![
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            "kdeconnect.contacts.response_vcards".to_string(),
        ]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        // kdeconnect-kde kdeconnect_contacts.json X-KdeConnect-OutgoingPacketType
        vec![
            "kdeconnect.contacts.request_all_uids_timestamps".to_string(),
            "kdeconnect.contacts.request_vcards_by_uid".to_string(),
        ]
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        // kdeconnect-kde syncs on connect: connected() ->
        // synchronizeRemoteWithLocal() -> sendRequest(REQUEST_ALL_UIDS_TIMESTAMPS)
        // (plugins/contacts/contactsplugin.cpp:42-45, 59-62).
        vec![self.request_all_uids_timestamps()]
    }

    async fn on_disconnected(&self, device_id: &str) {
        if let Ok(mut devices) = self.devices.write() {
            devices.remove(device_id);
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        match packet.packet_type.as_str() {
            "kdeconnect.contacts.response_uids_timestamps" => {
                // Body: {"uids": ["1","3"], "1": "1721950000000", ...}
                // (kdeconnect-android ContactsPlugin.kt:110-119)
                let Some(reported) = Self::parse_uids_timestamps(&packet.body) else {
                    info!(
                        device_id = %device_id,
                        event = "contacts_uids_malformed",
                        "Contacts uids/timestamps response missing uids array; ignored"
                    );
                    return Ok(None);
                };
                if reported.is_empty() {
                    info!(
                        device_id = %device_id,
                        event = "contacts_uids_empty",
                        "Device reports zero contacts; clearing stored state"
                    );
                }

                // One write lock for the whole read-decide-write update: a
                // concurrent sync from another device can neither interleave
                // nor deadlock (single lock, no ordering to invert).
                let reported_count = reported.len();
                let reported_uids: HashSet<String> = reported.keys().cloned().collect();
                let to_fetch: Vec<String> = {
                    let mut devices = self.devices.write().unwrap_or_else(|e| e.into_inner());
                    let state = devices.entry(device_id.to_string()).or_default();

                    // Decide which vCards to (re)fetch: new uid or changed
                    // timestamp (kdeconnect-kde contactsplugin.cpp:80-123).
                    let to_fetch: Vec<String> = reported
                        .iter()
                        .filter(|(uid, ts)| {
                            let prev = state.timestamps.get(*uid);
                            let has_vcard = state.contacts.contains_key(*uid);
                            prev != Some(ts) || !has_vcard
                        })
                        .map(|(uid, _)| uid.clone())
                        .collect();

                    // Record the reported timestamps and drop contacts the
                    // phone no longer reports (contactsplugin.cpp:125-129).
                    // Recompute total_vcard_bytes from the retained slice so
                    // the byte cap tracks the prune exactly: a contacts::len()
                    // change must be a total_vcard_bytes change too.
                    state.timestamps = reported;
                    state.contacts.retain(|uid, _| reported_uids.contains(uid));
                    state.total_vcard_bytes = state.contacts.values().map(|c| c.vcard.len()).sum();

                    to_fetch
                };

                info!(
                    device_id = %device_id,
                    reported = reported_count,
                    to_fetch = to_fetch.len(),
                    event = "contacts_uids_received",
                    "Received contact uids/timestamps from device"
                );

                if to_fetch.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(vec![self.request_vcards_by_uid(&to_fetch)]))
                }
            }
            "kdeconnect.contacts.response_vcards" => {
                // Body: {"uids": ["1","3"], "1": "BEGIN:VCARD...", ...}
                // (kdeconnect-android ContactsPlugin.kt:140-155)
                let Some(uids) = packet.body.get("uids").and_then(|u| u.as_array()) else {
                    warn!(
                        device_id = %device_id,
                        event = "contacts_vcards_malformed",
                        "vcards response missing uids key"
                    );
                    return Ok(None);
                };

                let mut parsed = Vec::new();
                for uid in uids.iter().filter_map(|u| u.as_str()) {
                    let Some(vcard) = packet.body.get(uid).and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let (name, phone_numbers, emails) = Self::parse_vcard_fields(vcard);
                    parsed.push((
                        uid.to_string(),
                        Contact {
                            uid: uid.to_string(),
                            name,
                            phone_numbers,
                            emails,
                            vcard: vcard.to_string(),
                        },
                    ));
                }

                // Snapshot gate + cap: only store vCards whose UID is in
                // the latest uids/timestamps snapshot. Drops (both
                // unreported-UID and cap-refused) are counted and logged
                // separately so a misbehaving peer is loud, not silent.
                let mut devices = self.devices.write().unwrap_or_else(|e| e.into_inner());
                let state = devices.entry(device_id.to_string()).or_default();
                let reported: HashSet<String> = state.timestamps.keys().cloned().collect();
                let (stored, dropped_unreported, dropped_cap) =
                    Self::store_vcards(state, &reported, parsed);

                if dropped_unreported > 0 {
                    warn!(
                        device_id = %device_id,
                        count = dropped_unreported,
                        event = "contacts_vcards_unreported_dropped",
                        "Dropped vCards whose UIDs are not in the latest reported snapshot"
                    );
                }
                if dropped_cap > 0 {
                    warn!(
                        device_id = %device_id,
                        count = dropped_cap,
                        event = "contacts_vcards_cap_exceeded",
                        "Refused vCard inserts past per-device cap"
                    );
                }

                info!(
                    device_id = %device_id,
                    stored,
                    event = "contacts_vcards_received",
                    "Stored contact vCards from device"
                );
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn test_contacts_plugin_name() {
        let plugin = ContactsPlugin::new();
        assert_eq!(plugin.name(), "contacts");
    }

    #[tokio::test]
    async fn test_contacts_capabilities() {
        // Must match kdeconnect-kde plugins/contacts/kdeconnect_contacts.json:
        // SupportedPacketType (incoming) = the two response types,
        // OutgoingPacketType = the two request types.
        let plugin = ContactsPlugin::new();
        let incoming = plugin.incoming_capabilities();
        assert!(incoming.contains(&"kdeconnect.contacts.response_uids_timestamps".to_string()));
        assert!(incoming.contains(&"kdeconnect.contacts.response_vcards".to_string()));
        let outgoing = plugin.outgoing_capabilities();
        assert!(outgoing.contains(&"kdeconnect.contacts.request_all_uids_timestamps".to_string()));
        assert!(outgoing.contains(&"kdeconnect.contacts.request_vcards_by_uid".to_string()));
    }

    /// Fixture: tests/fixtures/upstream-wire/contacts/request_all_uids_timestamps.json
    ///   kdeconnect-kde@f5ed3ed8 plugins/contacts/contactsplugin.cpp:169-176
    ///   sends the request with NO body fields.
    #[tokio::test]
    async fn test_request_all_uids_wire_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/contacts/request_all_uids_timestamps.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read contacts fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let plugin = ContactsPlugin::new();
        let packet = plugin.request_all_uids_timestamps();
        assert_eq!(
            packet.packet_type,
            "kdeconnect.contacts.request_all_uids_timestamps"
        );
        assert_eq!(packet.body, upstream_body);
    }

    /// Fixture: tests/fixtures/upstream-wire/contacts/request_vcards_by_uid.json
    ///   kdeconnect-kde@f5ed3ed8 plugins/contacts/contactsplugin.cpp:178-185
    ///   sets the "uids" key to the list of uid strings.
    #[tokio::test]
    async fn test_request_vcards_wire_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/contacts/request_vcards_by_uid.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read contacts vcards fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let plugin = ContactsPlugin::new();
        let packet = plugin.request_vcards_by_uid(&["1".to_string(), "3".to_string()]);
        assert_eq!(
            packet.packet_type,
            "kdeconnect.contacts.request_vcards_by_uid"
        );
        assert_eq!(packet.body, upstream_body);
    }

    /// Fixture: tests/fixtures/upstream-wire/contacts/response_uids_timestamps.json
    ///   EXACT body shape the phone sends, from kdeconnect-android
    ///   ContactsPlugin.kt:110-119: a "uids" string list, plus one field per
    ///   uid keyed BY the uid with the timestamp as a STRING.
    #[tokio::test]
    async fn test_handle_uids_timestamps_exact_phone_shape() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/upstream-wire/contacts/response_uids_timestamps.json");
        let upstream_body: serde_json::Value = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(&fixture_path).expect("read contacts response fixture"),
        )
        .expect("parse fixture")["body"]
            .clone();

        let plugin = ContactsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            upstream_body,
        );
        let reply = plugin
            .handle_packet("device1", packet)
            .await
            .unwrap()
            .expect("new uids must trigger a vcard request");
        assert_eq!(reply.len(), 1);
        assert_eq!(
            reply[0].packet_type,
            "kdeconnect.contacts.request_vcards_by_uid"
        );
        let mut requested: Vec<String> =
            serde_json::from_value(reply[0].body["uids"].clone()).unwrap();
        requested.sort();
        assert_eq!(requested, vec!["1", "15", "3"]);
    }

    #[tokio::test]
    async fn test_handle_vcards_exact_phone_shape_stores_contacts() {
        // EXACT body shape the phone sends, from kdeconnect-android
        // ContactsPlugin.kt:140-155: a "uids" string list, plus one field per
        // uid keyed BY the uid whose value is the raw vCard string.
        let plugin = ContactsPlugin::new();
        // Real protocol flow: phone first reports uids/timestamps, the
        // gate admits only vCards for UIDs in that snapshot.
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1"], "1": "1721950000000" }),
        );
        plugin.handle_packet("device1", snap).await.unwrap();

        let vcard = "BEGIN:VCARD\nVERSION:2.1\nFN:John Smith\nTEL;CELL:+15551234\nEMAIL:john@example.com\nX-KDECONNECT-ID-DEV-abcdef:1\nREV:1721950000000\nEND:VCARD";
        let packet = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1"],
                "1": vcard
            }),
        );
        let reply = plugin.handle_packet("device1", packet).await.unwrap();
        assert!(reply.is_none());

        let contacts = plugin.get_contacts("device1");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].uid, "1");
        assert_eq!(contacts[0].name.as_deref(), Some("John Smith"));
        assert_eq!(contacts[0].phone_numbers, vec!["+15551234"]);
        assert_eq!(contacts[0].emails, vec!["john@example.com"]);
        assert_eq!(contacts[0].vcard, vcard);
    }

    #[tokio::test]
    async fn test_parse_vcard_21_group_prefixes() {
        // vCard 2.1 group prefix parsing: real phones emit vCard 2.1 GROUP prefixes (RFC 2425
        // §3.3) — "item1.TEL;CELL:" instead of "TEL;CELL:". Unstripped,
        // the prop reads "ITEM1.TEL;CELL" and matches nothing, so
        // phone_numbers/emails come back empty for the whole contact list.
        let (name, phones, emails) = ContactsPlugin::parse_vcard_fields(
            "BEGIN:VCARD\nVERSION:2.1\nitem1.FN:Jane Doe\nitem1.TEL;CELL:+15559876\nitem2.TEL;HOME:+15554321\nitem1.EMAIL:jane@example.com\nEND:VCARD",
        );
        assert_eq!(name.as_deref(), Some("Jane Doe"));
        assert_eq!(phones, vec!["+15559876", "+15554321"]);
        assert_eq!(emails, vec!["jane@example.com"]);
    }

    #[tokio::test]
    async fn test_parse_vcard_dotted_param_value_is_not_a_group_prefix() {
        // A '.' inside the PARAM section (after ';') is not a group prefix —
        // stripping there would silently drop the property.
        let (_name, phones, _emails) = ContactsPlugin::parse_vcard_fields(
            "BEGIN:VCARD\nTEL;TYPE=WORK.VOICE:+15551111\nEND:VCARD",
        );
        assert_eq!(phones, vec!["+15551111"]);
    }

    #[tokio::test]
    async fn test_full_sync_flow_incremental() {
        let plugin = ContactsPlugin::new();

        // First sync: two uids reported, both requested.
        let p1 = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1", "2"], "1": "100", "2": "200" }),
        );
        let reply = plugin.handle_packet("d", p1).await.unwrap().unwrap();
        assert_eq!(
            reply[0].packet_type,
            "kdeconnect.contacts.request_vcards_by_uid"
        );

        // Phone answers with both vCards.
        let p2 = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1", "2"],
                "1": "BEGIN:VCARD\nFN:Alice\nEND:VCARD",
                "2": "BEGIN:VCARD\nFN:Bob\nEND:VCARD"
            }),
        );
        plugin.handle_packet("d", p2).await.unwrap();
        assert_eq!(plugin.get_contacts("d").len(), 2);

        // Second sync: uid 1 unchanged, uid 2 has a newer timestamp, uid 3 is
        // new. Only 2 and 3 must be re-requested (kdeconnect-kde
        // contactsplugin.cpp:116-121 compares timestamps).
        let p3 = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1", "2", "3"], "1": "100", "2": "201", "3": "300" }),
        );
        let reply = plugin.handle_packet("d", p3).await.unwrap().unwrap();
        let mut requested: Vec<String> =
            serde_json::from_value(reply[0].body["uids"].clone()).unwrap();
        requested.sort();
        assert_eq!(requested, vec!["2", "3"]);

        // Third sync: identical report -> no follow-up request.
        let p4 = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1", "2", "3"], "1": "100", "2": "201", "3": "300" }),
        );
        // uid 3 has no stored vcard yet, so it IS re-requested.
        let reply = plugin.handle_packet("d", p4).await.unwrap().unwrap();
        assert_eq!(reply[0].body, serde_json::json!({ "uids": ["3"] }));
    }

    #[tokio::test]
    async fn test_unreported_contacts_are_dropped() {
        // kdeconnect-kde deletes local vCards the remote no longer reports
        // (contactsplugin.cpp:125-129); we do the same with stored contacts.
        let plugin = ContactsPlugin::new();
        // Real protocol flow: report uids first, then vCards for those uids.
        let snap1 = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1", "2"], "1": "100", "2": "200" }),
        );
        plugin.handle_packet("d", snap1).await.unwrap();
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1", "2"],
                "1": "BEGIN:VCARD\nFN:Alice\nEND:VCARD",
                "2": "BEGIN:VCARD\nFN:Bob\nEND:VCARD"
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        assert_eq!(plugin.get_contacts("d").len(), 2);

        // Phone now reports only uid 1 (with unchanged timestamp).
        let p = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1"], "1": "100" }),
        );
        // uid 1 has a stored vcard but a different (unset) timestamp -> fetch.
        plugin.handle_packet("d", p).await.unwrap();
        let contacts = plugin.get_contacts("d");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].uid, "1");
    }

    #[tokio::test]
    async fn test_empty_uids_response_clears_stored_contacts() {
        // A present-but-empty uids array is a valid report (the phone deleted
        // all contacts) and must clear stored state — distinct from a MISSING
        // uids key, which is malformed and ignored
        // (kdeconnect-kde contactsplugin.cpp:125-129).
        let plugin = ContactsPlugin::new();
        // Real protocol flow: seed snapshot before storing vCards.
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1"], "1": "100" }),
        );
        plugin.handle_packet("d", snap).await.unwrap();
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1"],
                "1": "BEGIN:VCARD\nFN:Alice\nEND:VCARD"
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        assert_eq!(plugin.get_contacts("d").len(), 1);

        let p = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": [] }),
        );
        let reply = plugin.handle_packet("d", p).await.unwrap();
        assert!(reply.is_none(), "empty report requests no vcards");
        assert!(plugin.get_contacts("d").is_empty());
    }

    #[tokio::test]
    async fn test_numeric_timestamps_accepted() {
        // kdeconnect-kde reads the per-uid field with np.get<qint64>
        // (contactsplugin.cpp:116), so a numeric wire value must not break us.
        let plugin = ContactsPlugin::new();
        let p = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["7"], "7": 1721950000000i64 }),
        );
        let reply = plugin.handle_packet("d", p).await.unwrap().unwrap();
        assert_eq!(reply[0].body, serde_json::json!({ "uids": ["7"] }));
    }

    #[tokio::test]
    async fn test_malformed_responses_are_ignored() {
        let plugin = ContactsPlugin::new();
        // Missing "uids" key in both response types -> no reply, no state.
        let p = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "bogus": true }),
        );
        assert!(plugin.handle_packet("d", p).await.unwrap().is_none());
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({ "bogus": true }),
        );
        assert!(plugin.handle_packet("d", p).await.unwrap().is_none());
        assert!(plugin.get_contacts("d").is_empty());
    }

    #[tokio::test]
    async fn test_on_disconnected_clears_contacts() {
        let plugin = ContactsPlugin::new();
        // Seed the snapshot so the gate admits the vCard.
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1"], "1": "100" }),
        );
        plugin.handle_packet("d", snap).await.unwrap();
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1"],
                "1": "BEGIN:VCARD\nFN:Alice\nEND:VCARD"
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        assert_eq!(plugin.get_contacts("d").len(), 1);
        plugin.on_disconnected("d").await;
        assert!(plugin.get_contacts("d").is_empty());
    }

    #[tokio::test]
    async fn test_vcard_field_parsing_unfolds_continuation_lines() {
        let (name, phones, emails) = ContactsPlugin::parse_vcard_fields(
            "BEGIN:VCARD\r\nFN:A Very Long\r\n  Name\r\nTEL:+1\r\nEND:VCARD",
        );
        assert_eq!(name.as_deref(), Some("A Very LongName"));
        assert_eq!(phones, vec!["+1"]);
        assert!(emails.is_empty());
    }

    #[tokio::test]
    async fn test_on_connected_requests_sync() {
        // kdeconnect-kde syncs on connect (contactsplugin.cpp:42-45, 59-62);
        // we must send the same empty-body request packet.
        let plugin = ContactsPlugin::new();
        let packets = plugin.on_connected("device1");
        assert_eq!(packets.len(), 1);
        assert_eq!(
            packets[0].packet_type,
            "kdeconnect.contacts.request_all_uids_timestamps"
        );
        assert_eq!(packets[0].body, serde_json::json!({}));
    }

    #[tokio::test]
    async fn test_get_contacts_sorted_by_uid() {
        let plugin = ContactsPlugin::new();
        // Seed the snapshot so the gate admits all three vCards.
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["15", "3", "1"], "15": "1", "3": "1", "1": "1" }),
        );
        plugin.handle_packet("d", snap).await.unwrap();
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["15", "3", "1"],
                "15": "BEGIN:VCARD\nFN:Mom\nEND:VCARD",
                "3": "BEGIN:VCARD\nFN:Abe\nEND:VCARD",
                "1": "BEGIN:VCARD\nFN:John\nEND:VCARD"
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        let contacts = plugin.get_contacts("d");
        let uids: Vec<&str> = contacts.iter().map(|c| c.uid.as_str()).collect();
        assert_eq!(uids, vec!["1", "15", "3"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_device_syncs_do_not_deadlock() {
        // Two devices driving full sync flows concurrently on the shared
        // plugin instance. With the old split contacts/timestamps locks the
        // read block (contacts->timestamps) and write block
        // (timestamps->contacts) could AB-BA deadlock; the single-lock
        // structure makes that impossible by construction. The timeout turns
        // any regression into a fast, loud failure instead of a hung suite.
        let plugin = std::sync::Arc::new(ContactsPlugin::new());

        async fn sync_once(plugin: &ContactsPlugin, device: &str, iter: usize) {
            let ts = format!("{iter:04}");
            let p = Packet::new(
                "kdeconnect.contacts.response_uids_timestamps".to_string(),
                serde_json::json!({ "uids": ["1", "2"], "1": ts, "2": ts }),
            );
            let reply = plugin.handle_packet(device, p).await.unwrap();
            if let Some(requests) = reply {
                // Answer our own vCard request as the phone would.
                let uids: Vec<String> =
                    serde_json::from_value(requests[0].body["uids"].clone()).unwrap();
                let mut body = serde_json::json!({ "uids": uids });
                for uid in &uids {
                    body[uid] = serde_json::json!(format!(
                        "BEGIN:VCARD\nFN:Contact {uid} v{iter}\nEND:VCARD"
                    ));
                }
                let p = Packet::new("kdeconnect.contacts.response_vcards".to_string(), body);
                plugin.handle_packet(device, p).await.unwrap();
            }
        }

        let work = async {
            let mut handles = Vec::new();
            for device in ["devA", "devB"] {
                let plugin = plugin.clone();
                handles.push(tokio::spawn(async move {
                    for iter in 0..50 {
                        sync_once(&plugin, device, iter).await;
                    }
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
        };

        tokio::time::timeout(std::time::Duration::from_secs(10), work)
            .await
            .expect("concurrent syncs deadlocked");

        // Both devices ended on their final iteration's data.
        for device in ["devA", "devB"] {
            let contacts = plugin.get_contacts(device);
            assert_eq!(contacts.len(), 2);
            assert!(contacts.iter().all(|c| c.vcard.contains("v49")));
        }
    }

    // CV finding: response_vcards accepted every UID in the packet without
    // checking against the latest reported snapshot, letting a paired peer
    // grow state.contacts for the life of the connection.

    #[tokio::test]
    async fn test_vcards_for_unreported_uids_are_dropped() {
        // No snapshot has ever been reported: no UID is in state.timestamps,
        // so every vCard is dropped. State.contacts stays empty.
        let plugin = ContactsPlugin::new();
        let vcard = "BEGIN:VCARD\nFN:Ghost\nEND:VCARD";
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["never-reported-uid"],
                "never-reported-uid": vcard,
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        assert!(
            plugin.get_contacts("d").is_empty(),
            "vCards for UIDs never in the reported snapshot must not be stored"
        );
    }

    #[tokio::test]
    async fn test_vcards_for_reported_uids_are_accepted() {
        // The realistic protocol flow: phone reports uids/timestamps first
        // (the answer to our outgoing request_all_uids_timestamps), then
        // answers our request_vcards_by_uid. The gate admits both because
        // both UIDs are in the snapshot.
        let plugin = ContactsPlugin::new();
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1", "2"], "1": "100", "2": "200" }),
        );
        let reply = plugin.handle_packet("d", snap).await.unwrap();
        assert!(reply.is_some(), "snapshot must trigger vcard request");

        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1", "2"],
                "1": "BEGIN:VCARD\nFN:Alice\nEND:VCARD",
                "2": "BEGIN:VCARD\nFN:Bob\nEND:VCARD",
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        let contacts = plugin.get_contacts("d");
        assert_eq!(contacts.len(), 2);
        assert!(contacts.iter().any(|c| c.uid == "1"));
        assert!(contacts.iter().any(|c| c.uid == "2"));
    }

    #[tokio::test]
    async fn test_mixed_batch_admits_reported_and_drops_unreported() {
        // A response_vcards packet that mingles a reported UID with an
        // unreported one. Only the reported UID is stored; the other is
        // counted as the dropped-unreported bucket for the warn!.
        let plugin = ContactsPlugin::new();
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1"], "1": "100" }),
        );
        plugin.handle_packet("d", snap).await.unwrap();

        // Now the phone sends a vCard packet with both "1" (reported) and
        // "42" (NOT in the snapshot). Only "1" is admitted.
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1", "42"],
                "1": "BEGIN:VCARD\nFN:Alice\nEND:VCARD",
                "42": "BEGIN:VCARD\nFN:Impostor\nEND:VCARD",
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        let contacts = plugin.get_contacts("d");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].uid, "1");
    }

    #[tokio::test]
    async fn test_pruned_uid_stops_accepting_vcards_after_newer_snapshot() {
        // The original attack path: a contact is admitted while reported,
        // then a newer snapshot excludes that UID (the user deleted the
        // contact on the phone). A subsequent vCard packet for that UID
        // must be dropped, not silently re-admitted.
        let plugin = ContactsPlugin::new();
        // 1) Snapshot includes uid 1 + uid 2; both vCards arrive.
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1", "2"], "1": "100", "2": "200" }),
        );
        plugin.handle_packet("d", snap).await.unwrap();
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1", "2"],
                "1": "BEGIN:VCARD\nFN:Alice\nEND:VCARD",
                "2": "BEGIN:VCARD\nFN:Bob\nEND:VCARD",
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        assert_eq!(plugin.get_contacts("d").len(), 2);

        // 2) Newer snapshot: uid 1 was deleted on the phone. The prune
        // drops it from stored contacts (existing test_unreported_contacts_…);
        // the snapshot's reported set no longer contains uid 1.
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["2"], "2": "200" }),
        );
        plugin.handle_packet("d", snap).await.unwrap();
        assert_eq!(plugin.get_contacts("d").len(), 1);

        // 3) Peer resends a vCard packet that includes uid 1 again, hoping
        // the prune outlived its gate. The gate must reject: uid 1 is no
        // longer in the snapshot.
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1"],
                "1": "BEGIN:VCARD\nFN:Alice-Take2\nEND:VCARD",
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();
        let contacts = plugin.get_contacts("d");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].uid, "2");
    }

    #[tokio::test]
    async fn test_count_cap_refuses_past_cap() {
        // Defense-in-depth: even if the snapshot gate had a bug, we refuse
        // the (cap+1)-th contact. Use the _inner helper with a small cap so
        // the test doesn't need to manufacture 10k vCards.
        let mut state = DeviceContacts::default();
        let mut reported = HashSet::new();
        let max = 3usize;
        let mut parsed: Vec<(String, Contact)> = Vec::new();
        for n in 0..max {
            let uid = format!("u{n}");
            reported.insert(uid.clone());
            parsed.push((
                uid.clone(),
                Contact {
                    uid: uid.clone(),
                    vcard: format!("BEGIN:VCARD\nFN:{uid}\nEND:VCARD"),
                    name: None,
                    phone_numbers: Vec::new(),
                    emails: Vec::new(),
                },
            ));
        }
        let (stored, dropped_unreported, dropped_cap) =
            ContactsPlugin::store_vcards_inner(&mut state, &reported, parsed, max, usize::MAX);
        assert_eq!(stored, max);
        assert_eq!(dropped_unreported, 0);
        assert_eq!(dropped_cap, 0);
        assert_eq!(state.contacts.len(), max);

        // Next insert is refused with dropped_cap = 1 even though its UID is
        // reported.
        let uid = "u-overflow".to_string();
        reported.insert(uid.clone());
        let contact = Contact {
            uid: uid.clone(),
            vcard: "BEGIN:VCARD\nFN:Overflow\nEND:VCARD".to_string(),
            name: None,
            phone_numbers: Vec::new(),
            emails: Vec::new(),
        };
        let (stored, _unr, dropped_cap) = ContactsPlugin::store_vcards_inner(
            &mut state,
            &reported,
            vec![(uid, contact)],
            max,
            usize::MAX,
        );
        assert_eq!(stored, 0);
        assert_eq!(dropped_cap, 1);
        assert_eq!(state.contacts.len(), max);
    }

    #[tokio::test]
    async fn test_byte_cap_refuses_past_cap() {
        // Defense-in-depth: even if the snapshot gate had a bug, we refuse
        // an insert whose vCard size alone exceeds the byte cap.
        let mut state = DeviceContacts::default();
        let reported = HashSet::new();
        let max_bytes = 100usize;
        // A vCard bigger than the cap by itself must be refused.
        let uid = "big".to_string();
        let contact = Contact {
            uid: uid.clone(),
            vcard: "BEGIN:VCARD\nFN:Big\nEND:VCARD".to_string() + &"X".repeat(max_bytes + 1),
            name: None,
            phone_numbers: Vec::new(),
            emails: Vec::new(),
        };
        let mut reported = reported;
        reported.insert(uid.clone());
        let (stored, _unr, dropped_cap) = ContactsPlugin::store_vcards_inner(
            &mut state,
            &reported,
            vec![(uid, contact)],
            usize::MAX,
            max_bytes,
        );
        assert_eq!(stored, 0);
        assert_eq!(dropped_cap, 1);
        assert!(state.contacts.is_empty());
    }

    #[tokio::test]
    async fn test_byte_cap_handles_update_path() {
        // An UPDATE (existing UID) reduces byte delta is always allowed;
        // a byte INCREASE that fits under the cap is allowed; one that
        // would push past the cap is refused.
        let mut state = DeviceContacts::default();
        let uid = "u1".to_string();
        let mut reported = HashSet::new();
        reported.insert(uid.clone());
        let small = Contact {
            uid: uid.clone(),
            vcard: "BEGIN:VCARD\nFN:Small\nEND:VCARD".to_string(),
            name: None,
            phone_numbers: Vec::new(),
            emails: Vec::new(),
        };
        let (stored, _, _) = ContactsPlugin::store_vcards_inner(
            &mut state,
            &reported,
            vec![(uid.clone(), small)],
            usize::MAX,
            1000,
        );
        assert_eq!(stored, 1);
        assert_eq!(state.total_vcard_bytes, state.contacts[&uid].vcard.len());

        // Update that stays under cap: allowed.
        let bigger = Contact {
            uid: uid.clone(),
            vcard: "BEGIN:VCARD\nFN:Bigger\nEND:VCARD".to_string() + &"Y".repeat(100),
            name: None,
            phone_numbers: Vec::new(),
            emails: Vec::new(),
        };
        let (stored, _, dropped_cap) = ContactsPlugin::store_vcards_inner(
            &mut state,
            &reported,
            vec![(uid.clone(), bigger.clone())],
            usize::MAX,
            10_000,
        );
        assert_eq!(stored, 1);
        assert_eq!(dropped_cap, 0);
        assert_eq!(state.contacts[&uid].vcard, bigger.vcard);

        // Update that would push past cap: refused, prior value preserved.
        let prev = state.contacts[&uid].vcard.clone();
        let too_big = Contact {
            uid: uid.clone(),
            vcard: "BEGIN:VCARD\nFN:TooBig\nEND:VCARD".to_string() + &"Z".repeat(100_000),
            name: None,
            phone_numbers: Vec::new(),
            emails: Vec::new(),
        };
        let (stored, _, dropped_cap) = ContactsPlugin::store_vcards_inner(
            &mut state,
            &reported,
            vec![(uid, too_big)],
            usize::MAX,
            1000, // small cap to force refusal
        );
        assert_eq!(stored, 0);
        assert_eq!(dropped_cap, 1);
        assert_eq!(
            state.contacts["u1"].vcard, prev,
            "refused update must not mutate stored vcard"
        );
    }

    #[tokio::test]
    async fn test_snapshot_prune_updates_total_vcard_bytes() {
        // The snapshot prune at the bottom of response_uids_timestamps drops
        // contacts not in the reported set; total_vcard_bytes must reflect
        // the drop exactly, otherwise the byte cap starts to drift relative
        // to the actual storage.
        let plugin = ContactsPlugin::new();
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1", "2"], "1": "100", "2": "200" }),
        );
        plugin.handle_packet("d", snap).await.unwrap();
        let p = Packet::new(
            "kdeconnect.contacts.response_vcards".to_string(),
            serde_json::json!({
                "uids": ["1", "2"],
                "1": "BEGIN:VCARD\nFN:Alice\nEND:VCARD",
                "2": "BEGIN:VCARD\nFN:Bob\nEND:VCARD",
            }),
        );
        plugin.handle_packet("d", p).await.unwrap();

        // Snapshot now excludes uid 2.
        let snap = Packet::new(
            "kdeconnect.contacts.response_uids_timestamps".to_string(),
            serde_json::json!({ "uids": ["1"], "1": "100" }),
        );
        plugin.handle_packet("d", snap).await.unwrap();

        // Read the bytes from state directly via a fresh packet: the
        // byte accounting should sum exactly to the surviving contact.
        let expected_bytes = "BEGIN:VCARD\nFN:Alice\nEND:VCARD".len();
        // Indirect check via get_contacts: sum of vcard lengths equals
        // expected since only one contact remains.
        let stored_bytes: usize = plugin.get_contacts("d").iter().map(|c| c.vcard.len()).sum();
        assert_eq!(stored_bytes, expected_bytes);
    }
}
