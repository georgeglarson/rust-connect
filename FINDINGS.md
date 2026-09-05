# FINDINGS — fix-cert-san-deviceid adversarial review (GLM-5.3 round, vk #1045)

Branch: `fix-certsan-glm-review` (review branch off `fix-cert-san-deviceid`
@ 3df6008, which sits on `e1b57aa`). The implementer's FINDINGS.md is
preserved at commit 3df6008; this file is the review round's artifact.

Bottom line: the branch's SAN fix did NOT fix the 2026-08-14 defect. The
live oracle — run for the first time ever in this round, against the
source-built kdeconnectd the implementer claimed did not exist —
reproduced the exact Qt rejection WITH the fix's cert in the handshake.
The SAN carried the device id verbatim (dashes); kdeconnectd verifies
the D-Bus-normalized id (underscores); Qt's match is exact. Fixed here,
red-to-green on the claim's own scenario, plus four more confirmed
findings (a 50%-flake serial encoding bug, and three defects in the m5
oracle script that made it unable to see any of this).

## What changed

Six commits on the review branch:

1. **`f7350fc` crypto: pin encoded serial to exactly 20 octets
   (finding #0, CONFIRMED by the integrator).** `serial_buf[0] =
   (serial_buf[0] & 0x7F) | 0x01` plus a comment stating the DER
   padding, the leading-zero strip, and the RFC 5280 4.1.2.2 20-octet
   cap. The instruction's literal fix (`&= 0x7F` only) is insufficient:
   yasna's `write_bigint_bytes` strips leading zero bytes before
   deciding to pad, so a masked-to-0x00 first byte encodes to 19 octets
   (~1/256 per cert, proven deterministically). Forcing the low bit
   pins the encoding at exactly 20 every time; 158 random bits remain.

2. **`94b371b` crypto: SAN dNSName carries the D-Bus-normalized device
   id (finding 1, THE top finding).** `generate_certificate` maps the
   id through the same rule kdeconnectd applies
   (`filterNonExportableCharacters`: every char outside [A-Za-z0-9_]
   becomes `_`) before pushing the dNSName. For hex/underscore ids
   (Android parity) the normalized form IS the id verbatim. CN, RDN
   order, OU/O, validity, serial scheme untouched — CN deliberately
   stays the RAW id (kdeconnectd's addLink normalizes both sides of its
   comparison; our own `own_certificate_cn` expects the raw string).
   Tests: new `test_generated_cert_san_is_kde_normalized_id` (R4),
   R1's expectation updated to the normalized form for its dashed test
   id, R2 (underscore id, no-op normalization) unchanged.

3. **`596d229` interop m5: fix the oracle defects (findings 2-4).**
   Phase 3's rejection check was polarity-INVERTED (it passed when the
   rejection text was present and failed with a lying detail when
   absent) and racy (single grep immediately after restart; the
   rejection lands ~1s later). The scenario itself was
   dial-race-dependent and could silently prove nothing. Phase 3 now
   nudges kde to re-broadcast (provoking rust's UDP-path dial, the only
   dial that carries a real port), WAITS for kdeconnectd's TLS-client
   path to actually run (dies loudly otherwise), settles 3s, then
   asserts absence with correct polarity. Phase 6 grepped for a
   `ping_response_received` event that exists nowhere in the codebase —
   replaced with two honest oracles (REST `sent:true` + packet receipt
   in the kde log). Dead pseudo-shell helper removed.

4. **`f38218c` interop m2: resolution note** in the m2_smoke.sh
   skip-rationale block, per the brief's step 5 (historical text kept,
   resolution + date added).

5. **`bed49d5` interop: m5 reachable through run.sh (finding 5).**
   `run.sh m5` errored ("unknown milestone") and the runner resolves
   `${MILESTONE}_smoke.sh` while the file was `m5_restart_kde.sh` —
   renamed, case/usage/`all` wired. run.sh also now derives
   LD_LIBRARY_PATH from RC_KDECONNECTD's install dir and passes it
   through the sudo exec: the source build's RUNPATH was baked to its
   since-deleted build worktree (`rc-3.2-m4`), and sudo's env_reset
   strips LD_LIBRARY_PATH, so the reference binary could not resolve
   `libkdeconnectcore.so.26` via the runner.

6. This file.

## How it was verified

The implementer's DEFERRED claim was false on both stated facts: the
source build exists at
`tests/interop/.kde/install/bin/kdeconnectd` (v26.04.3 @ c687cf11,
SOURCE_MANIFEST.toml alongside, in the MAIN checkout — gitignored, so
the worktree fooled the lane), and `sudo -n` works. Everything below
ran from this worktree.

### Finding #0 — serial encoding (red → green, statistical + deterministic)

Scratch test over 100 real `generate_certificate` runs, pre-fix:

    SCRATCH TALLY: {20: 53, 21: 47}        (cargo test --lib, FAILED by design)

Deterministic writer semantics (one keypair, hand-picked serial bytes,
yasna 0.6 `write_bigint_bytes(bytes, positive=true)` via rcgen 0.14.9):

    first=0xff second=0x11 -> encoded serial 21 octets   (the flake)
    first=0x00 second=0x42 -> encoded serial 19 octets   (the case a bare mask misses)
    first=0x00 second=0x80 -> encoded serial 20 octets
    first=0x01 second=0x42 -> encoded serial 20 octets
    first=0x7f second=0x42 -> encoded serial 20 octets

Post-fix: `SCRATCH TALLY: {20: 100}`. R3 run 10x post-fix: 10 pass,
0 fail. rcgen's own default serial path does only `sl[0] &= 0x7f`
(certificate.rs:455) and accepts variable length ≤ 20; R3's `== 20`
pin is stricter, hence the low-bit force.

### Finding 1 — the SAN value (red → green on the claim's own live scenario)

Three live m5 runs against the source-built reference (isolated netns
harness; production daemons untouched; zero-leak PASS every run):

**Run 1 (pre-fix binary, pre-review script) — RED, partially masked.**
Phase 2 failed the cert-shape check: SAN `c049921c-5981-...` (dashed)
vs expected `c049921c_5981_...` (the id as kde knows it). Phase 3
"failed" while the log contained NO rejection (the inverted check).
Phase 6 timed out (phantom oracle). The link itself came up — because
kdeconnectd won the dial race and ran TLS-server (`Starting server ssl
(I'm the client TCP socket)`), which never hostname-checks. Rust's own
outbound dial had failed earlier on a broken address: mDNS resolves the
peer as `10.250.137.2:0` — port zero — ECONNREFUSED, then
reverse-connection fallback. The defect path never ran; the run was
green-shaped for the wrong reason.

**Run 3 — RED, the defect itself, with the fix's cert.** Rust's UDP-path
dial (correct port) went out ~1s after kde's restart:

    rust:   12:30:06.050 initiating_outgoing_connection ... 10.250.64.2:1716
    rust:   12:30:06.054 TLS handshake completed successfully (server role)
    kde:    8:30:06.050 LanLinkProvider newTcpConnection
    kde:    8:30:06.051 Starting client ssl (but I'm the server TCP socket)
    kde:    8:30:06.055 Disconnecting due to fatal SSL Error: "The host
            name did not match any of the valid hosts for this certificate"
    rust:   12:30:06.055 Failed outgoing connection attempt ... peer closed
            connection without sending TLS close_notify

That is the 2026-08-14 defect verbatim, firing under the branch's fix.
Mechanism, from the kdeconnect-kde source on this host: the TCP dialer
runs TLS server; the TCP ACCEPTOR runs TLS client with
`configureSslSocket` → `setPeerVerifyName(deviceId)`
(lanlinkprovider.cpp:604), where `deviceId` comes from the identity
packet AFTER `NetworkPacket::unserialize` normalization
(networkpacket.cpp:82-87 → dbushelper.cpp:31: `[^A-Za-z0-9_]` → `_`).
Verify name = underscore form; branch's SAN = dashed form; exact-match
hostname comparison → fatal. Unit tests R1/R2 could not catch this:
they assert SAN == the input id, self-consistently blind to the form
the peer verifies against.

**Run 4 (post-fix binary + fixed script) — GREEN end-to-end.** `M5
SMOKE: PASS`, exit 0: SAN = underscore form; TLS-client path ran at
8:39:31.268 and `Socket successfully established an SSL connection` at
.274; rust logged `tls_server_handshake_complete` +
`outgoing_connection_success`; no rejection line anywhere; pair states
and TOFU store intact; ping POST `sent:true` and the packet delivered
(kde: `discarding unsupported packet "kdeconnect.ping"` — the plugin-less
source reference logs receipts that way).

**Run 5 — GREEN through the canonical entrypoint.**
`RC_KDECONNECTD=... tests/interop/run.sh m5` → `M5 SMOKE: PASS`,
ZERO-LEAK PASS, exit 0. Second consecutive full pass.

### Red-test honesty (flip)

SAN push temporarily removed from the committed generator:

    test_generated_cert_validity_and_serial_unchanged_after_san ... ok
    test_generated_cert_san_is_kde_normalized_id ... FAILED
    test_generated_cert_carries_device_id_san ... FAILED
    test_generated_cert_san_accepts_underscore_id ... FAILED

R3 passes both ways — a regression pin, not a behavior test, exactly as
the implementer said (point 7), and it is the pin that surfaced
finding #0. Restored to byte-identical (git diff clean) after the run.

### DER-level diff (desk item)

Both the pre-fix and post-fix certs carry EXACTLY ONE X509v3 extension
— the SAN. No BasicConstraints/KeyUsage/SKI rode along. The only
pre/post delta is the SAN's value (dash → underscore). The generator's
textual delta vs main `e1b57aa` is the SAN push (plus comment/tests);
rcgen emits the SAN extension iff `subject_alt_names` is non-empty.

### Cert-shape consumers (desk item)

No production code reads the SAN (grep: only the generator and three
tests). `own_certificate_cn` / `extract_cn_from_der` read CN —
unchanged, so identity recovery is unaffected. Android's
`getCommonNameFromCertificate` reads CN (present, unchanged) and its
TLS layer is trust-all (SslHelper.kt:53-57), so extension presence is
inert there. kdeconnectd's addLink compares
`subjectDisplayName()`-normalized vs packet-id-normalized
(lanlinkprovider.cpp:637-644) — empirically fine with SAN present
(run 4/5 pairing + link-up) and insensitive to dash/underscore either
way. `verify_peer_fingerprint` on mismatch returns an error and
REJECTS the connection (crypto.rs:828-844); only `unpair()` deletes
(cert-change semantics: refuse + re-pair, nothing subtler). The kde
side under a cert change rejects and cascades — but this branch
deliberately never regenerates existing identities, so no existing
pairing ever sees a change from it.

### Gates

- `cargo test --all-features --locked --no-fail-fast` with
  `set -o pipefail`, no TMPDIR override,
  `CARGO_TARGET_DIR=/home/glitchenstein/repos/rust-connect/target`:
  **1375 passed, 0 failed** across all targets.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`:
  clean. `cargo fmt --check`: clean.
- One cargo build at a time throughout; no artifacts in the tmpfs
  worktree (the temporary `target` symlink used for the run.sh
  validation was removed; tree clean).
- `shellcheck -S error` clean on m5_smoke.sh and run.sh.

## Critique — blunt

**The brief specified the wrong value for the only line that mattered.**
"dNSName carrying the device id verbatim" is precisely what
kdeconnectd rejects for dashed ids. The brief anchored on
lanlinkprovider.cpp:604 but never followed the `deviceId` argument's
dataflow back through `NetworkPacket::unserialize`'s D-Bus
normalization — two upstream files it otherwise cites. The test spec
inherited the error ("R1 — SAN present with exactly one dNSName equal
to the device_id"), so the lane produced green unit tests over a fix
that provably did not work. A unit-level oracle cannot catch this
class of bug by construction; only the live oracle could. Which makes
the brief's ordering — "Interop repro — best-effort live oracle (do
this LAST)", with an explicit license to DEFER when the build is
inconvenient — exactly backwards for a change the brief itself classes
as trust-core-adjacent. The live run should be the merge gate, not a
trailing nicety. This round is the existence proof: every substantive
finding (1, 2, 3) came from running it, and zero came from re-reading
the diff.

**The DEFERRED escape hatch was taken on unchecked facts.** "No
source-built kdeconnectd: ... does not exist" and "not root" were each
falsifiable in one command (`ls` on the .kde install; `sudo -n true`),
and both were false — the build lives in the main checkout, which a
gitignored-paths worktree silently hides. The brief's timebox language
("do not burn the lane on Qt build fights") is fine; deferral without
mandating the two-second fact checks is not. A lane that defers the
live oracle on a claim nobody verified shipped a broken fix to review.

**The brief's serial instruction was also one branch short.**
`serial_buf[0] &= 0x7F` does not yield "always exactly 20 octets": the
DER writer strips leading zeros before padding, so the
masked-to-0x00 case encodes to 19 octets with probability ~1/256 per
cert — the proposed statistical green test would itself have been
flaky ~1/3 of the time at N=100. The instruction's stated goal was
right; its mechanism hadn't been checked against the writer.

**What the brief got right, and where I tried to break it and could
not:** the SAN approach itself (a dNSName SAN is what Qt's
peerVerifyName verification reads; run 4/5 prove it works with the
normalized value); no forced regeneration of existing identities
(correct — regeneration would fire the rejection cascade on every
existing kde pairing); leaving `delete_peer_certificate` semantics
alone (the cascade dies with its trigger, now for real); not patching
upstream. The extension diff confirmed nothing rode along with the
SAN. The consumer sweep found no reader that prefers the raw form.

**Adjudication of the implementer's seven critique points:**

1. `rcgen::string::Ia5String` path — correct, upheld.
2. OID constants in `oid_registry`, not `prelude` — correct, upheld.
3. `.to_string()` on the borrowed `&str` — cosmetic, upheld (left
   as-is; the intent is clearer on read).
4. ASN1Time/openssl double-parse for the time pin — upheld, option (b)
   as chosen; a helper would be surface area for nothing.
5. Log-offset mechanics — mechanically upheld (lib.sh's restart_kde
   appends, `wc -l` + `sed` slicing is sound), but the critique
   examined the wrong thing entirely: the check built on that offset
   was polarity-inverted AND the oracle it fed was a phantom event
   name AND the scenario could degenerate into proving nothing. A
   critique that validates the plumbing while never running the water
   misses everything that mattered. Finding 2/3 supersede.
6. Future Android-side hostname hardening — upheld as
   document-don't-code, with stronger grounds than the implementer
   had: the DESKTOP peer already requires the normalized form today,
   and the generator comment now documents the normalization with
   cites. An Android that hardened against raw ids would equally break
   every Android-to-Android pairing (SslHelper certs are CN-only), so
   it is not a realistic near-term threat model for us specifically.
7. R3-is-a-pin-not-a-behavior-test — upheld, and vindicated: the pin
   is what surfaced finding #0. Pins that look redundant are exactly
   what catches latent regressions when adjacent code changes.

**Pre-existing defects surfaced, deliberately NOT fixed here** (outside
the branch's diff; recorded for the backlog):

- The mDNS discovery path resolves peers with port 0 (`10.250.x.y:0`,
  and at least once `127.0.0.1:0`), so every mDNS-triggered outbound
  dial fails with instant ECONNREFUSED and falls back to
  reverse-connection. This masked the SAN defect in run 1 and makes
  rust's outbound dials structurally unreliable in mDNS-first
  environments. src/protocol/mdns_discovery.rs:243 +
  connection_orchestrator.rs:344.
- `send_packet`'s capability gate (src/protocol/connection/mod.rs:~500)
  refuses when the peer's recorded incomingCapabilities are an EMPTY
  list, while its doc comment promises refusal only when they are
  "NON-EMPTY and don't list" the type. Empirically moot in these runs
  (the ping flowed), but the doc/code drift is real and will bite the
  first plugin-facing flow against a plugin-less peer.
- The source-built kdeconnectd loads no plugins in the harness
  environment (identity announces `incomingCapabilities:[]`), so
  m3/m5 plugin-level flows exercise the plugin-less shape of the peer;
  a distro-binary run would behave differently. Recorded, not acted on.

**What will break in production that the tests do not cover:** the m5
oracle now depends on rust's UDP-path dial provocation; if the mDNS
port-0 defect is ever "fixed" by making mDNS dials work, the
dial-direction economics change and Phase 3's provocation should still
hold (the nudge is UDP-based), but the `Starting client ssl`
wait-for-die is the tripwire that will say so loudly. TLS 1.3 is
untested against this reference (observed handshakes were 1.2); Qt's
hostname check is version-independent, so the risk is low but the
coverage note stands.
