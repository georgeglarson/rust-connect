# Security & resource-lifetime audit — 2026-08 (Sprint 2, Task 2.5)

Four independent read-only lanes (network/TLS/pairing, payload/file/resource,
API auth/CORS/secrets, CVE/dependency) audited the daemon against
`docs/threat-model.md`. Findings were treated as leads; each disposition below
was verified against code (file:line) and, for the one High finding,
reproduced red-before-green before fixing.

**Bottom line:** one High finding (L2-1, fixed this cycle). Of the rest, one is
Medium (F1, a TLS-layer-vs-application-layer invariant divergence that is not a
bypass) and the remainder are Low or config-gated. Dependency tree carries zero
runtime-reachable vulnerabilities. Every historical KDE Connect CVE is closed in
this reimplementation.

Method caveat: the CVE lane's `/tmp/kdeconnect-kde` reference was a shallow
single-commit clone, so upstream git-log archaeology was not possible; KDE's
published security-advisory feed (the authoritative source) was used instead.

## Fixed this cycle

### L2-1 — unbounded pre-auth registry growth [High, class A]

A LAN peer reached a persistable registry record with zero pairing:
identity-exchange drives `lifecycle.ensure_and_transition(id, Connected)`
before any pairing check, `Device::is_paired()` treats `Connected`/`Disconnected`
as persistence-eligible (a naming collision — real pairing lives in
`PairingHandler.paired`), `registry.add()` persists on every add, and
`prune_stale_devices` ran only at startup with no `MAX_DEVICES` cap. Churning
fresh random device-ids grew the in-memory map and `devices.json` without
bound, pre-auth — matching the rubric's "unbounded resource exhaustion
reachable pre-auth."

**Fix:** durability now follows real pairing. The registry holds a shared clone
of `PairingHandler`'s paired-ids handle; `save_to_disk` filters on true pairing,
not the state predicate; unpaired records are bounded by `MAX_UNPAIRED_DEVICES`
with LRU-by-`last_seen` eviction that never drops a truly-paired device; a mass
unpair that pushes the unpaired count over the cap is drained back on the next
insert (not evicted one-at-a-time). Reproduced red-before-green in
`src/device/registry.rs` unit tests
(`test_flood_of_unpaired_devices_is_capped_and_not_persisted`,
`test_truly_paired_device_survives_unpaired_flood`,
`test_eviction_drops_oldest_unpaired_by_last_seen`,
`test_persistence_gate_uses_true_pairing_not_connected_state`,
`test_cap_bounds_growth_with_no_paired_handle_wired`,
`test_mass_unpair_then_insert_drains_back_to_cap`), each confirmed failing
against pre-fix code before the fix landed. This also subsumes CVE-2020-26164
item 1f (spoofed-UDP-broadcast device-record accumulation), which the CVE lane
punted here.

## Ledger — accepted residual risks (Low / config-gated, not fixed this cycle)

Each is a real observation kept as an explicit entry per the plan's "lower
risks remain explicit ledger entries." None meets the fix-before-release bar
(unauth compromise / secret leak / RCE / unbounded pre-auth exhaustion).

### F1 — outbound TLS client-auth is a post-handshake check, not a TLS-layer pin [Medium, class A]
`outbound.rs` calls `tls::tls_accept(..., None, ...)` with a hardcoded `None`
device-id, so `TofuClientCertVerifier::client_auth_mandatory` is dead on the
main device link — fingerprint verification there is always the post-handshake
app-layer check (`outbound.rs:299-319`), never the TLS-layer hard pin the module
doc claims. Not a bypass (rejection still happens, no packets processed first),
but the "checked during handshake, no post-hoc window" invariant holds only for
the inbound role. Worth closing later by threading the known device-id into the
outbound `tls_accept` call. Confidence: high.

### F2 — pairing `max_pending` (10) check-then-insert TOCTOU [Low, class A]
Concurrent pairing requests from distinct spoofed device-ids can each observe
`total < max_pending` before any inserts, nudging the pending map past the soft
cap. Bounded by concurrent-TLS-handshake cost; mild pending-state amplification,
not unbounded growth. Fix would make the check-and-insert atomic under one
write-lock. Confidence: medium.

### F3 — own-cert expiry never checked on the inbound accept path [Low, class A]
`ensure_own_certificate` runs at startup and on outbound dials, never before
presenting our cert on `accept_incoming`. A receive-only daemon could present a
technically-expired cert until restart. Low/likely-intentional: TOFU ignores
X.509 validity dates entirely, so expiry has no cryptographic bearing here.
Confidence: medium.

### API-1 — `/docs` and `/api-docs/openapi.json` served without auth [Low-Medium, class C]
The auth middleware is layered on `api_router` before SwaggerUi is merged onto
the outer router, so the Swagger UI and full OpenAPI schema serve
unauthenticated (`router.rs:246-249`). No secret in the schema, but it is
undocumented recon surface if the API is ever rebound off loopback. One-line
fix (wrap SwaggerUi in the auth layer, or gate it off-loopback). Confidence:
high. **Disposition: deferred to a fast-follow, not in this PR** — kept out to
keep the trust-boundary change reviewable in isolation; tracked for the next
API-hardening pass.

### API-2 — API key accepted via `?api_key=` query string [Low, class C]
Necessary for browser `EventSource` (can't set headers) for the SSE stream. The
app's own request logger strips query strings before logging, so no first-party
leak; the key can still land in browser history or a reverse-proxy access log
if the deployment shape changes. Compounds the existing "treat the key as
compromised if rebound to LAN" note. Confidence: high.

### API-3 — length side-channel on the constant-time key comparison [Low, class C]
`constant_time_contains` skips the XOR loop for length-mismatched keys with no
dummy work, so total time leaks whether any configured key shares the probe's
length. Practically useless against a 122-bit UUID-v4 key on a loopback/same-user
model where disk-read already beats timing. Confidence: medium.

### API-4 — single-key, no-scopes authorization model [Low, class C — plan item (c)]
**Disposition: intentional divergence, documented, not scoped now.** Under the
default loopback bind, a same-user attacker who could use the key can already
read the key file (mode 0600) and the identity keys directly — scoping buys
nothing against that adversary. The blast-radius argument (one flat token
controls pairing, SFTP mount, input injection, remote-command, SMS send, and
the authenticated outbound-dial primitive at `device.rs:429`) becomes real only
on a LAN rebind, where the threat model already says "generate a fresh key" —
and on that rebind the key is a single flat capability token with no
compartmentalization across those operations. If ever scoped, the natural first
cut is read-only vs control. Not Sprint-2 budget.

## Confirmed bounded (independently verified against threat-model claims)

Network/TLS/pairing: TOFU hard-pinned during the handshake on the we-are-client
role; trust storage gated by real pairing, not connection; cert CN bound to
claimed device-id both roles; changed-cert-vs-stored-fingerprint hard-fails; 512
KiB pre-auth identity cap enforced during read; protocol-downgrade /
targetDeviceId / mid-handshake identity change all rejected; every reader of the
pairing pending-maps filters `is_expired`; per-IP outbound dial rate limit;
RFC1918/loopback/link-local/CGNAT private-address filtering on inbound + UDP.

Payload/file/resource: streaming `payloadSize=-1` cap enforced as-read with
partial-delete on abort; no fd/task/permit leaks (incl. the announce-but-never-
open-port case, tested); path traversal defeated by basename-flatten +
`create_new` (symlinked-intermediate and final-component escapes both closed);
runcommand peer input is only a lookup key, never reaches the shell; SFTP mount
points daemon-derived; notification-icon path strictly validated.

API/auth: all 50 sensitive routes behind the API-key extractor; SSE
authenticated; SFTP password never in any response/log/argv (redacted Debug,
stdin-only); rate limiter keys on the real TCP peer (not header-spoofable); CORS
`*` never sets `allow_credentials` (deny-by-default, opt-in).

Dependencies (cargo-audit 0.22.2 / cargo-deny 0.20.2): 0 vulnerabilities across
475 crates; the only unmaintained-crate warnings (paste, proc-macro-error,
quick-xml) are proc-macro/build-time-only or absent from the Linux resolve —
none runtime-reachable. quick-xml's O(N²) attribute DoS goes live only for a
Windows build (0.41.0+ is the fix if that ever ships). Licenses/sources/yanked
all clean.

Historical KDE Connect CVEs, all closed in this reimplementation:
CVE-2025-66270 (device-id mismatch impersonation — the critical one) closed by
the TLS-authenticated second identity exchange with an explicit id-match check;
CVE-2025-32898 (static SAS) closed by the v8 timestamped SAS;
CVE-2025-32899 (discovery-triggered unpair) not applicable — unpair only over an
established TLS link; CVE-2025-32900 (spoofable discovery info) closed by v8
TLS identity exchange; CVE-2020-26164 bundle: memory-safety items N/A (safe
Rust), the DoS items (size caps, connection cap, line-read bound) all fixed by
construction, item 1f fixed as part of L2-1 above; historical share path
traversal closed by `sanitize_filename`.
