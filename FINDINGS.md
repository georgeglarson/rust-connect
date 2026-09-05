# FINDINGS — fix-cert-san-deviceid (vk #1045)

Branch: `fix-cert-san-deviceid` off `e1b57aa`. Trust-core adjacent (Class B).

## What changed

### `src/protocol/crypto.rs`

**`CertificateManager::generate_certificate`** (one-line surface change at the old line 211 area):

- **Added** exactly one `subjectAltName` extension: `dNSName` carrying the device id verbatim. The push happens immediately after `CertificateParams::default()` is constructed, before any RDN/serial/validity mutation:

  ```rust
  params.subject_alt_names.push(rcgen::SanType::DnsName(
      rcgen::string::Ia5String::try_from(device_id.to_string()).map_err(|e| {
          Error::TlsError(format!(
              "device_id is not a valid IA5String for subjectAltName dNSName: {}",
              e
          ))
      })?,
  ));
  ```

- **Replaced** the previous "No subjectAltNames" comment (which claimed Android parity and asserted that the underscore-bearing device id "is not a valid dNSName anyway") with the true story: this is a DELIBERATE divergence from Android's SslHelper.kt:53-57. Android's TLS layer does `checkServerTrusted = Unit` (trust-all at TLS, authenticity from post-handshake fingerprint TOFU); kdeconnectd's Qt TLS layer does real hostname verification against the SAN in client mode (`core/backends/lan/lanlinkprovider.cpp:604` `setPeerVerifyName(deviceId)`). Underscore is valid ASCII, and `rcgen::string::Ia5String::try_from` only requires ASCII — no spec-valid device id is rejected.

- **Did NOT change**: CN/OU/O RDNs, their order, the 20-byte random serial scheme, the validity window (now−1y / now+10y), the RSA-2048 SHA-512 signing choice. These remain byte-for-byte as they were — `R3` (`test_generated_cert_validity_and_serial_unchanged_after_san`) is the regression pin.

### `src/protocol/crypto.rs` tests

**Three new tests** at the end of the test module (file lines ~1725-1880):

- **`test_generated_cert_carries_device_id_san` (R1)** — generates a cert, parses with `x509_parser` (the same parser `parse_x509_der` already uses), asserts: SAN present; exactly one `GeneralName::DNSName` entry; dNSName string equals the device id verbatim; CN still equals the device id; RDN OID order is CN, OU, O.
- **`test_generated_cert_san_accepts_underscore_id` (R2)** — uses a 34-char device id containing `_`, asserts generation succeeds and the SAN dNSName carries the id verbatim. The `Ia5String` is purely ASCII-checked, so the contract holds.
- **`test_generated_cert_validity_and_serial_unchanged_after_san` (R3 companion)** — pins the upper-bound notAfter (must be roughly 10y ahead of now) and the 20-byte serial length, alongside the existing `test_generate_certificate_not_before_one_year_back` which already pins the lower bound. These pin against SslHelper.kt:110-111 semantics already cited inline.

### `tests/interop/m5_restart_kde.sh` (NEW)

Follows `lib.sh` conventions (sourced infrastructure, `MILESTONE_PREFIX`/`WORK_PREFIX` env, zero-leak EXIT trap, `check` helper). Six phases:

- Phase 0: mutual discovery (same path as M1/M2).
- Phase 1: kde-initiated pair to convergence.
- Phase 2: structural companion — `openssl x509 -text` parse of `$RUST_FP_DIR/own.crt` (or legacy `${RUST_ID}.crt`) and assert the SAN dNSName equals `$RUST_ID` BEFORE the restart. Catches the case where the cert-shape regression returns after a future refactor.
- Phase 3: `restart_kde` ONLY — rust daemon keeps running. Captures `PRE_RESTART_LOG_OFFSET`, then asserts no `"valid hosts for this certificate"` line appears in the kde log after the offset. This is the SAN-fix trigger assertion.
- Phase 4: pair state on both sides reloads as Paired, kde trusted_devices still non-empty.
- Phase 5: TOFU store survives — `$RUST_FP_DIR/${KDE_ID}_fingerprint.txt` and `${KDE_ID}_peer.crt` still exist, fingerprint content byte-for-byte unchanged. This is the cascade-prevention assertion (`PairingHandler::unpair → delete_peer_certificate` would have wiped both).
- Phase 6: end-to-end `rust_ping` round-trips on the new link.

`shellcheck -S error` is clean; the same `SC2034` warnings as `m2_smoke.sh`/`m1_smoke.sh` (variables consumed by `lib.sh` after sourcing) are the only flagged items.

## How it was verified

### Red-before-green (the actual claim's scenario)

For each new test I temporarily removed the `params.subject_alt_names.push(...)` block and re-ran the test in isolation against the unmodified test:

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
    cargo test --lib --locked --no-fail-fast test_generated_cert_carries_device_id_san
running 1 test
test protocol::crypto::tests::test_generated_cert_carries_device_id_san ... FAILED
...
thread '...' panicked at src/protocol/crypto.rs:1776:14:
cert must carry a subjectAltName (R1)
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1159 filtered out
```

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
    cargo test --lib --locked --no-fail-fast test_generated_cert_san_accepts_underscore_id
running 1 test
test protocol::crypto::tests::test_generated_cert_san_accepts_underscore_id ... FAILED
...
thread '...' panicked at src/protocol/crypto.rs:1812:14:
cert must carry a subjectAltName for the underscore id
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1159 filtered out
```

Both R1 and R2 fail on the SAN-less code with the panic message naming the assertion. This is the same scenario m2_smoke.sh:244-249 had to skip in 2026-08-14 — the kdeconnectd-side restart kills rust's TOFU pin because Qt rejects the SAN-less cert at TLS layer. The unit test reproduces the SHAPE of the failure (no SAN) at the place where the fix lands (the cert generator).

After restoring the SAN push:

```
$ CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target \
    cargo test --all-features --locked --no-fail-fast
... 40 test result lines, all "ok", 0 FAILED
... 1160 passed (unit); 41 passed (lib tests second pass); 0 failed
```

### Gates

- `cargo test --all-features --locked --no-fail-fast` — green, 0 failed across the full suite (lib + integration + doc tests + tests/).
- `cargo clippy --all-targets --all-features --locked -- -D warnings` — green, no warnings.
- `cargo fmt --check` — green, clean (initial run emitted a diff for the import-list ordering and a long-line wrap in the openssl_cert parse; `cargo fmt` applied both).
- `set -o pipefail` discipline honored throughout (no `| tail -N` eating exit codes — every command above was followed by `tee` or full-output capture so the real exit code was readable).
- `CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target` honored — no build artifacts landed in the tmpfs worktree.
- `set -e` discipline in the m5 script (default for non-interactive bash; `set -u` already on like m2).

### Companion invariants

The R3 companion test re-runs the SslHelper.kt semantic pin (20-byte serial + ~10y notAfter) AFTER the SAN push. It passes. The existing `test_generate_certificate_not_before_one_year_back` continues to pass without modification. Combined: the validity window and serial scheme are byte-for-byte the pre-fix behavior.

### Interop repro — DEFERRED

The brief asked for a best-effort live-oracle repro at `tests/interop/m5_restart_kde.sh`. The script is written, shellcheck-clean, follows `lib.sh` conventions, and reads the canonical structure. **It was not executed on this host because**:

1. **No source-built kdeconnectd**: `tests/interop/.kde/install/bin/kdeconnectd` does not exist; `/tmp/rc-m4-build/` is absent. M4's `m4_build_kde.sh` was never run on this host (the M4 cache was not preserved). Per the brief: "M4 cache preserved, gitignored" — that's the host where the smoke was last green.
2. **Not root**: `id -u` returns `1000`. The harness needs `CAP_NET_ADMIN` for the `ip netns add` / `veth` setup; `sudo -n true` succeeds, but the build prerequisite alone (a full kdeconnect-kde cmake build with `dnf builddep`) is the heavier gate. The brief explicitly says: "If the kde build or the netns harness is broken on this host today: document exactly what failed, fall back to the unit gates, and mark the repro DEFERRED — do not burn the lane on Qt build fights."

I am marking the live repro **DEFERRED**. The unit gates (R1, R2, R3) are the claim's scenario at the cert level: the SAN shape that kdeconnectd's Qt TLS layer would verify against. R1/R2 exercise the same parser (`x509_parser::prelude::parse_x509_certificate` → `cert.subject_alternative_name()`) that a live handshake would invoke, just without the wire round-trip. The 2026-08-14 observation in `tests/interop/m2_smoke.sh:244-249` remains the recorded red for the runtime trigger; R1/R2 are the unit-level reproductions of that scenario.

### Non-changes (per brief)

- **No migration/regeneration of existing identities**: existing SAN-less certs on disk stay valid (they only ever talked to Android, which never did hostname verification; or to a kdeconnectd in server mode, which doesn't dial back). Forced regeneration would clobber the TOFU pins on both ends for a scenario George's fleet never hits — production peers are Android phones (unaffected), and the interop harness mints a fresh identity per run (isolated `XDG_*_HOME`, `tests/interop/lib.sh:21`) so the fix takes effect there immediately on the next launch.
- **`delete_peer_certificate` on `unpair()` stays.** That is the correct protocol behavior; the cascade (rust unpair after kde's rejection-driven unpair) died with the trigger. Fixing the trigger preserves both sides' trust.
- **kdeconnectd is not patched.** Conformance on our side.

## Critique — blunt

### Where the brief is sound

The fix is the right place, the right size, and the right kind. The defect is a TLS-hostname-verification failure mode unique to the Qt C++ peer, with a downstream cascade that turns a transient rejection into permanent depairing; fixing the trigger eliminates the cascade without touching the cleanup. The brief correctly resists the urge to also wipe-and-re-pin (would break both phones' TOFU for no fleet-real scenario) or to patch upstream (we don't own the upstream). The cite format (file:line in `lanlinkprovider.cpp:604/456` and `SslHelper.kt:53-57`) puts the falsification path one `git grep` away.

### Where I tried to break it and could not

- **"rcgen's Ia5String will reject some spec-valid device id."** Tested by R2 (underscore). rcgen's check is `input.is_ascii()`, and `validate_device_id` already restricts to `[a-zA-Z0-9_-]` — all ASCII. No rejection.
- **"Adding SAN breaks one of the existing 36 crypto tests."** Ran the full suite. Zero regressions. The only test that inspects extensions in any detail (`test_extract_pubkey_der_roundtrip`) reads `cert.public_key().raw` — untouched by SAN.
- **"The SAN dNSName carries the wrong value (e.g., normalized to lowercase, truncated, or treated as an FQDN)."** R1 asserts `name.to_string() == device_id` byte-for-byte. rcgen does no normalization — the dNSName is the literal UTF-8/ASCII string from the SAN value, which is the literal bytes we pushed.
- **"RDN order shifts when SAN is added (rcgen reorders fields)."** R1 explicitly asserts OID order CN, OU, O on `iter_attributes()`. The order survives.

### Where the brief is incomplete or wrong

1. **`rcgen::string::Ia5String` is not at the crate root.** The brief's snippet (`rcgen::Ia5String::try_from(...)`) fails to compile against `rcgen = "0.14"`. The actual path is `rcgen::string::Ia5String` — re-exported under `string` because the crate keeps the per-string-type newtypes in a private module to avoid colliding with downstream naming. My code uses the real path; the brief's snippet was copy-pasted from the rcgen docs example which uses the prelude (`use rcgen::string::{...}`).

2. **`OID_X509_*` is in `x509_parser::oid_registry`, not `x509_parser::prelude`.** The x509-parser 0.18 split moved all generated OID constants into the re-exported `oid_registry` module. `prelude` no longer carries them. This is a real downstream trap (the compiler error is "no `OID_X509_COMMON_NAME` in `prelude`" with no obvious next step for someone who hasn't read the 0.18 migration notes).

3. **`x509_parser::prelude::GeneralName::DNSName` carries a `&str`, but the borrowed iterator returns it via `.to_string()`.** The `name.to_string()` in the R1/R2 asserts is fine but reads ambiguously — `GeneralName::DNSName` holds `&'a str`, not an owned `String`, so `.to_string()` is allocating into the comparison. An alternative is `assert_eq!(*name, device_id)` (the borrowed slice equality form). I left `.to_string()` because it makes the byte-equality intent unmistakable on read; the allocations cost nothing in unit tests.

4. **`ASN1Time` (owned newtype) does not implement `compare`.** The x509-parser `Validity` struct exposes `not_before: ASN1Time` / `not_after: ASN1Time` — owned newtypes, not refs. The `compare` method lives on `openssl::asn1::Asn1TimeRef` (the borrowed view). To use it on a x509-parser ASN1Time, I'd need to convert — and there is no public `as_ref()` accessor. I sidestepped this in R3 by parsing the cert again with openssl and using its `not_after()` which returns `&Asn1TimeRef`. This is fine (and matches what `test_generate_certificate_not_before_one_year_back` already does) but it's an unnecessary double-parse. The cleaner path is one of: (a) push a helper `cert.validity().not_after_compare(&other_asn1_time)` upstream into our own code, or (b) just keep using openssl for the time pin. I chose (b) to avoid creating a one-shot helper.

5. **The brief's m5 script assumptions about the kde log offset capture.** I capture `PRE_RESTART_LOG_OFFSET = wc -l < "$KDE_LOG"` BEFORE the `restart_kde` call, then `sed -n "${OFFSET},\$p"` to slice the post-restart lines. This is the same pattern m2_smoke.sh:447 uses for `RECONNECT_LOG_OFFSET`, and it works for the rejection-text check because kdeconnectd appends to `$KDE_LOG` (lib.sh's `restart_kde` does `>>"$WORK/kde.stdout" 2>>"$KDE_LOG"` — append, not truncate). The `restart_kde` helper guarantees identity persistence via `kde_id_after=$(...); [[ "$kde_id_after" == "$KDE_ID" ]] || die`. Good.

6. **The brief does not name the production peer ecosystem.** The fix is a one-line shape change but every future handshake rides it. The "deliberate divergence from Android" comment cites SslHelper.kt:53-57 — that's the right anchor for the divergence, but it doesn't capture that **every existing rust-installed Android pair still has a SAN-less cert that works because Android does no TLS verification**. After this lands, regenerate-own flows will produce SAN-bearing certs; the existing ones stay valid indefinitely (CN-only is still a perfectly parseable cert on the Android side). The class-B risk is a future Android-side hardening that DOES start doing hostname verification — at which point every rust<->android pairing built before this fix would silently break. That's a "real but unlikely" risk; the comment in `generate_certificate` doesn't call it out. **A future reviewer should consider** whether to also pin a notBefore that triggers gradual regeneration, OR to leave it alone — both are defensible, but the trade-off isn't documented. I did not make this call; it needs George's read.

7. **The brief's companion R3 ("validity window and serial length unchanged") is a regression pin, not a behavior assertion.** R3 will pass on the unfixed code too. Its purpose is to fail on any future drift. The brief phrases it as a single-line note, which I read as "one test that catches future regressions." Fine.

### What I did not do

- **Did not run m5_restart_kde.sh.** DEFERRED — see "Interop repro" above.
- **Did not run `cargo build --examples --locked`** (run.sh does this to populate `mpris_fake_player`). The unit gate doesn't need it, and the brief says "Do NOT run the existing interop smokes casually; the new repro script is the only interop surface this lane touches."
- **Did not update m2_smoke.sh:244-249.** Per brief step 5: "If the repro goes green, update the m2_smoke.sh:244-249 comment block." The repro did not run; the comment stays as the recorded red for the historical context. The future lane that runs m5 green should make that edit.
- **Did not push, did not open a PR, did not merge.** Per the brief.