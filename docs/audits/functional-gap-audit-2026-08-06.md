# Functional gap audit — 2026-08-06 (Slice 0B, Task 0.5)

Independent black-box gap audit. Executor: GLM-5.2 headless lane (different
provider than the Task 0.4 implementation lane, per plan routing). RECON only:
no production code, test, or config was changed. The sole written artifact is
this report.

## 1. Scope, method, instance facts

**Mission.** Start from what a skeptic can see — the published binary, the REST
API, the web UI, the served OpenAPI document, and the public docs (`README.md`,
`SECURITY.md`, `ROADMAP.md`, `KDECONNECT_PROTOCOL.md`, `docs/threat-model.md`)
— without trusting internal docs, ledger self-descriptions, or test-suite
claims. Find, reproduce, record. Map every finding to the ledger
(`docs/functional-coverage.md`) or mark it NEW-GAP / out-of-ledger.

**Method.**
1. Built a fresh release binary (`cargo build --release --locked`). The prebuilt
   `target/release/rust-connect` (mtime 2026-08-06 09:29) was **stale**: the
   `ad4adae` / `cbc9fdc` commits touched `src/plugins/{battery,lock,...}.rs`
   after it was built, and `--version` reports only the generic `0.1.0` (no
   commit hash), so mtime was the only match signal. Rebuilt (1m39s, clean).
2. Ran a throwaway instance against a throwaway data dir, bound off the
   daily-driver ports. Probed only this instance via loopback HTTP.
3. Used the served OpenAPI doc as the authoritative route list; cross-checked
   every doc'd route live; established the auth baseline; then probed error
   handling, headers/CORS, rate limiting, SSE, info disclosure, config knobs,
   and the prior-audit threat list.
4. The installed daily-driver daemon was observed with read-only operations
   only (`ss`, `/proc/<pid>/cmdline`, read-only GETs). No POST/PUT/DELETE,
   `systemctl`, `adb`, or phone interaction.

**Instance facts.**

| field | value |
|---|---|
| Binary | `target/release/rust-connect`, version `0.1.0` |
| Built from | HEAD `cbc9fdc6` on branch `slice-0b-blackbox-audit` (== `main`) |
| Build cmd | `cargo build --release --locked` (clean, 1m39s) |
| Data dir | `RUST_CONNECT_DATA_DIR=/tmp/rc-audit-0b-data` (throwaway) |
| API bind | `127.0.0.1:19090` (via `--api-port 19090`; default loopback bind confirmed) |
| KDE protocol | tcp/udp `11716` (via `--port 11716`, to avoid the daily driver on `1716`) |
| Device name | `rc-audit-0b` (via `--device-name`); device id `90512447-…` |
| API key file | `/tmp/rc-audit-0b-data/api_key`, mode `0600` ✓, UUIDv4, length 36 |
| Lifespan | wrapped in `timeout 5400`; no `kill`/`pkill` available to the lane |
| Plugins loaded | 24; capabilities 29 incoming / 24 outgoing |
| Backends up (this env) | clipboard (Wayland/wl-clipboard), mpris (zbus, found "Brave"), systemvolume (pactl, 2 sinks), mousepad+presenter (uinput), screensaver-inhibit |
| mDNS discovery | discovered the daily driver ("laptop-RustConnect") + two real phones ("Galaxy A15 5G", "Galaxy S21 Ultra 5G") on the LAN and connected **unpaired** at the TLS layer automatically |

**Hands-off boundary.** The throwaway instance auto-connected (unpaired) to real
LAN devices via mDNS. The real phones were treated as hands-off: all device-scoped
POST/DELETE probing used a **synthetic** device id (`zzzz0000-0000-0000-0000-000000000000`).
No pair / unpair / share / lock / input packet was ever sent to a real device id.
`$KEY` in all reproducers is the throwaway API key, redacted.

## 2. Findings

Severity scale: critical / high / medium / low / nit.

### F-M1 — `DELETE …/unpair` returns HTTP 500 for a not-paired device (medium)

- **Surface:** REST API, `DELETE /api/v1/devices/{device_id}/unpair`.
- **Claim vs observed:** Unpairing a device that is not paired is a benign,
  expected client-state condition. The endpoint returns **HTTP 500** with body
  `{"error":{"code":"DEVICE_NOT_PAIRED","message":"Device not paired: …"}}`. The
  error *code* is a client-error code, but the *HTTP status* is a server error.
  The sibling `DELETE /api/v1/devices/{device_id}` returns `404 NOT_FOUND` for
  the same id, and `DELETE …/sftp/mount` returns `404`. Unpair is the outlier.
- **Reproducer:**
  ```
  $ curl -s -o - -w '\n%{http_code}\n' -X DELETE -H "X-API-Key: $KEY" \
      http://127.0.0.1:19090/api/v1/devices/zzzz0000-0000-0000-0000-000000000000/unpair
  {"status":"error","error":{"code":"DEVICE_NOT_PAIRED","message":"Device not paired: zzzz0000-0000-0000-0000-000000000000"},"metadata":{…}}
  500
  ```
- **Ledger:** lifecycle area (row at `functional-coverage.md:88`; `unpair` refs
  at `:419`, `:1952`). NEW-GAP — no row tracks unpair HTTP-status semantics.
- **Disposition proposal:** Map the `DEVICE_NOT_PAIRED` condition to **409
  Conflict** (or 404), not 500. Reserve 500 for unexpected internal errors.
  Auth is correct (the endpoint is key-gated); the defect is the status code.
- **Why medium:** a 500 on a benign precondition misleads monitoring/error
  budget tooling into treating a client mistake as a server crash, and the code
  already contradicts the status.

### F-M2 — Unknown-device handling is inconsistent across the whole API (medium)

- **Surface:** REST API, device-scoped routes.
- **Claim vs observed:** For an unknown / unconnected device id, the same
  logical precondition ("device doesn't exist / isn't connected") yields five
  different outcomes depending on endpoint:
  - `GET …/devices/{id}` → **404** `NOT_FOUND` (correct)
  - `GET …/devices/{id}/{battery,connectivity,remotecommands,sftp}` → **404** (correct)
  - `GET …/devices/{id}/{contacts,mpris,state,telephony}` and
    `…/{id}/sms/threads[/{thread_id}]` → **200** with empty data, e.g.
    `{"contacts":[],"count":0,"device_id":"zzzz0000-…"}` — **no existence check**
  - `POST …/pair` → **200** `pairing_initiated` (see F-L3)
  - `POST …/{disconnect,findmyphone,share/send,sftp/request,contacts/sync}` → **400** `INVALID_REQUEST` ("Device is not connected")
  - `POST …/{sms/request,battery/request,clipboard/request,mpris/request}` → **503** `CONNECTION_ERROR`
  - `POST …/remotecommands/{key}/trigger` → **404** `DEVICE_NOT_FOUND`
  - `DELETE …/unpair` → **500** `DEVICE_NOT_PAIRED` (F-M1)
- **Reproducer (200-on-bogus-device):**
  ```
  $ curl -s -H "X-API-Key: $KEY" \
      http://127.0.0.1:19090/api/v1/devices/zzzz0000-0000-0000-0000-000000000000/contacts
  {"status":"ok","data":{"contacts":[],"count":0,"device_id":"zzzz0000-0000-0000-0000-000000000000"},"metadata":{…}}
  ```
- **Ledger:** out-of-ledger. The ledger tracks per-feature `api_surface`
  dimensions, not a cross-cutting "uniform device-not-found contract." NEW-GAP.
- **Disposition proposal:** Adopt one contract: an unknown device id returns
  `404 DEVICE_NOT_FOUND` (or 400) at the routing/validation layer before the
  handler, so `contacts`/`mpris`/`state`/`telephony`/`sms` stop fabricating
  `200`+empty for non-existent devices, and the POST paths stop splitting across
  400/404/500/503.
- **Why medium:** a client cannot distinguish "device has no contacts" from
  "device does not exist" on five endpoints; the split status codes make
  automated consumers brittle. No security impact (all paths are auth-gated).

### F-L1 — Malformed input returns un-enveloped `text/plain` errors leaking parser internals (low)

- **Surface:** REST API, JSON-body POST handlers + the rate limiter.
- **Claim vs observed:** README:157-158 states "Responses follow
  `{ status, data, metadata }` format. Errors use structured codes like
  `DEVICE_NOT_FOUND`." But malformed/missing/typed JSON returns **plain
  `text/plain`** bodies outside the envelope, leaking serde/axum extractor
  internals and field names:
  - empty body → `400 Failed to parse the request body as JSON: EOF while parsing a value at line 1 column 0`
  - bad JSON → `400 …key must be a string at line 1 column 2`
  - wrong type → `422 Failed to deserialize the JSON body into the target type: device_id: invalid type: integer \`12345\`, expected a string at line 1 column 18`
  - rate-limited → `429 Rate limit exceeded` (also plain text)
- **Reproducer:**
  ```
  $ curl -s -D - -o - -X POST -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
      -d '{"device_id":12345}' http://127.0.0.1:19090/api/v1/ping
  HTTP/1.1 422 Unprocessable Entity
  content-type: text/plain; charset=utf-8
  …
  Failed to deserialize the JSON body into the target type: device_id: invalid type: integer `12345`, expected a string at line 1 column 18
  ```
- **Ledger:** out-of-ledger. NEW-GAP.
- **Disposition proposal:** Wrap extractor rejections in the standard
  `{status,error,metadata}` envelope with a structured code (e.g.
  `INVALID_JSON` / `VALIDATION_ERROR`) and a sanitized message. Low sensitivity,
  but it is a doc-truth contradiction and an inconsistent error contract.

### F-L2 — README claims `/api/v1/devices` lists *paired* devices; it lists all (low, doc-truth drift)

- **Surface:** README:144 vs `GET /api/v1/devices`.
- **Claim vs observed:** README:144 reads "`GET /api/v1/devices` — List paired
  devices." The live endpoint returns **every** known device with no pairing
  filter — on this instance, 3 devices, all `pair_state: not_paired`,
  `paired_at: null`.
- **Reproducer:**
  ```
  $ curl -s -H "X-API-Key: $KEY" http://127.0.0.1:19090/api/v1/devices
  {"status":"ok","data":{"devices":[{"id":"…","name":"laptop-RustConnect","device_type":"desktop","state":"connected","paired_at":null,"pair_state":"not_paired"}, … {"name":"Galaxy A15 5G", … "pair_state":"not_paired"}, …],"total":3},"metadata":{…}}
  ```
- **Ledger:** out-of-ledger. NEW-GAP.
- **Disposition proposal:** Either fix the README ("List known/connected
  devices") or add a pairing filter. Feeds Task 5.4 (doc-truth drift).

### F-L3 — `POST …/pair` returns `200 pairing_initiated` with no device-existence check (low)

- **Surface:** REST API, `POST /api/v1/devices/{device_id}/pair`.
- **Claim vs observed:** Pairing a non-existent device id returns
  `200 {"data":{"device_id":"zzzz0000-…","status":"pairing_initiated"}}` — a
  success envelope with no validation that the device is known. Every other
  device-scoped route validates the id (404/400/503).
- **Reproducer:**
  ```
  $ curl -s -X POST -H "X-API-Key: $KEY" \
      http://127.0.0.1:19090/api/v1/devices/zzzz0000-0000-0000-0000-000000000000/pair
  {"status":"ok","data":{"device_id":"zzzz0000-0000-0000-0000-000000000000","status":"pairing_initiated"},"metadata":{…}}
  ```
- **Ledger:** lifecycle area (row `functional-coverage.md:88`). NEW-GAP.
- **Disposition proposal:** Validate device existence before acknowledging
  pairing (return 404 for unknown ids). Note: no packet reached a real peer (the
  id is synthetic); with a *real* unpaired id this endpoint is the intended
  pairing trigger, so this is a validation/consistency gap, not a vuln.

### F-L4 — Doubled "Invalid API request: Invalid API request:" prefix (low)

- **Surface:** REST API, device_id path validation messages.
- **Claim vs observed:** Invalid device_id paths produce a doubled prefix:
  `{"error":{"code":"INVALID_REQUEST","message":"Invalid API request: Invalid
  API request: device_id: device_id must be 32-38 characters"}}`. The
  "Invalid API request:" wrapper is applied twice.
- **Reproducer:**
  ```
  $ curl -s -H "X-API-Key: $KEY" http://127.0.0.1:19090/api/v1/devices/aaaa
  {"status":"error","error":{"code":"INVALID_REQUEST","message":"Invalid API request: Invalid API request: device_id: device_id must be 32-38 characters"},"metadata":{…}}
  ```
- **Ledger:** out-of-ledger. NEW-GAP (cosmetic error-construction defect).
- **Disposition proposal:** Apply the wrapper once. Side note: the message also
  discloses the accepted id length (32-38 chars) — trivial, but fold into the
  same fix.

### F-L5 — README under-documents the API (9 vs 51 routes) and omits the per-IP REST rate limit (low, doc-truth drift)

- **Surface:** README "API" section; rate-limit documentation.
- **Claim vs observed:**
  - README:144-152 lists **9** endpoints. The served OpenAPI doc advertises
    **51** routes (full device/plugin surface: sftp, sms, clipboard, mpris, lock,
    volume, notification actions, contacts, systemvolume, etc.). README:170-179
    does say the web UI "exposes every device endpoint, every plugin action," so
    this is a quick-start gap, not a hidden surface — but a skeptic reading only
    the README sees a far smaller API than exists.
  - README:192 and `SECURITY.md` mention only **pairing** rate limiting ("max 10
    concurrent pending"). The REST API **also enforces a per-IP rate limit**
    (see Positive verifications): after a burst (~56 requests) it returns `429
    Retry-After: 60`, pre-auth. `docs/threat-model.md:74` correctly documents
    "Per-IP API rate limiting"; the README/SECURITY omission is the drift.
- **Reproducer (rate limit confirmed enforced):**
  ```
  $ for i in $(seq 1 250); do curl -s -o /dev/null -w '%{http_code}\n' -H "X-API-Key: $KEY" \
      http://127.0.0.1:19090/api/v1/devices; done | sort | uniq -c
       56 200
      194 429
  $ curl -s -i http://127.0.0.1:19090/api/v1/devices   # while limited
  HTTP/1.1 429 Too Many Requests
  retry-after: 60
  Rate limit exceeded
  ```
- **Ledger:** out-of-ledger. NEW-GAP.
- **Disposition proposal:** Expand the README API table (or point at the served
  OpenAPI/Swagger as authoritative) and document the per-IP REST rate limit.

### F-L6 — `/api/v1/tools` emits a duplicate tool entry (low)

- **Surface:** REST API, `GET /api/v1/tools`.
- **Claim vs observed:** The tools list has 10 entries but only 9 unique names;
  `get_telephony` is listed twice (identical).
- **Reproducer:**
  ```
  $ curl -s -H "X-API-Key: $KEY" http://127.0.0.1:19090/api/v1/tools \
      | jq '.data.tools | {total:length, unique:([.[].name]|unique|length), dups:([group_by(.name)[]|select(length>1)|.[0].name])}'
  {"total": 10, "unique": 9, "dups": ["get_telephony"]}
  ```
- **Ledger:** out-of-ledger. NEW-GAP.
- **Disposition proposal:** Deduplicate the tools registry.

### F-L7 — `--api-key` value is visible in `/proc/<pid>/cmdline` (low; documented/accepted P3, reproduced)

- **Surface:** CLI flag; known P3 from the 2026-08-03 audit.
- **Claim vs observed:** README:110-116 explicitly documents this. Reproduced
  unchanged (not fixed): a key passed via `--api-key` is readable by any local
  user in `ps` / `/proc/<pid>/cmdline`.
- **Reproducer (controlled throwaway, `--no-api`, separate port):**
  ```
  $ RUST_CONNECT_DATA_DIR=/tmp/rc-audit-0b-psprobe timeout 30 \
      target/release/rust-connect --no-api --port 11717 --api-key 'AAABBB-VisibleInPs-Marker-12345' &
  $ tr '\0' ' ' < /proc/$(pgrep -f rust-connect|head -1)/cmdline
  /usr/bin/…/rust-connect --no-api --port 11717 --api-key AAABBB-VisibleInPs-Marker-12345
  ```
- **Daily driver is clean:** `/proc/<daily-driver-pid>/cmdline` is just
  `/usr/bin/rust-connect` — the installed daemon reads the key file and does
  **not** pass `--api-key`, so the installed instance is not exposed.
- **Ledger:** out-of-ledger. Not a new gap (documented accepted risk).
- **Disposition proposal:** No action required (documented). The default
  key-file path (`~/.local/share/rust-connect/api_key`, mode 0600) and
  `RUST_CONNECT_API_KEY` already avoid it; reserve the flag for throwaway keys,
  per README.

### F-N1 — OpenAPI doc does not mark `/api/v1/health` as public (nit)

- **Surface:** Served OpenAPI (`/api-docs/openapi.json`).
- **Claim vs observed:** `README:155` says health is the one keyless `/api/v1`
  route, and live it is. But the served OpenAPI doc leaves health with
  `security: null` (inherits the top-level `api_key` requirement) rather than an
  explicit `security: []` override — so the doc implies health needs auth when it
  does not. Safe direction (doc stricter than reality).
- **Reproducer:** `jq '.paths."/api/v1/health"' /api-docs/openapi.json` →
  `get.security: null`; live `GET /api/v1/health` with no key → `200`.
- **Ledger:** out-of-ledger. NEW-GAP (doc/live agreement).
- **Disposition proposal:** Add `security: []` to the health operation.

### F-N2 — `/` redirects to `/ui/`, README says `/ui` (nit, doc-truth drift)

- **Surface:** README:174 vs `GET /`.
- **Claim vs observed:** README:174 "open http://localhost:9090/ (which
  redirects to `/ui`)". Live `GET /` → `302 location: /ui/` (trailing slash).
- **Ledger:** out-of-ledger.
- **Disposition proposal:** README s/`/ui`/`/ui/`/.

## 3. Unconfirmed observations (no black-box reproducer)

These could not be reproduced black-box from the throwaway instance; each has a
proposed experiment for the integrator.

- **U1 — Unpaired-peer plugin-packet reach (PR #15 fix).** Cannot be reached via
  the REST API (device-scoped POSTs check connection/pair state and refuse), and
  completing a KDE Connect TLS handshake requires a real peer with a matching
  identity exchange. Ledger row `cad-pair-false-on-unpaired-traffic`
  (`functional-coverage.md:586`) is `api_surface: PASS`.
  *Experiment:* point a second rust-connect (or kdeconnect-kde) instance at the
  daemon over loopback, pair is *not* completed, and attempt to deliver a
  non-pair packet; assert it is dropped before plugins.
- **U2 — Share staging path behavior (symlinks / collisions / traversal, PR #15).**
  `share/send` to a bogus device returns `INVALID_REQUEST: Device is not
  connected`, so the staging logic was unreachable without a paired device, and
  `/api/v1/share/files` was empty. *Experiment:* pair a device, then
  `POST …/share/send` a file whose name contains `../`, a symlink, or a NUL; and
  a collision with an existing staged name; assert the on-disk landing path stays
  inside the staging root.
- **U3 — `/api/v1/tools` honesty when a backend is absent.** All ten tools
  reported `available: true` on this instance, and every backend they depend on
  *was* present (`sshfs` at `/usr/bin/sshfs`, wl-clipboard, pactl with 2 sinks,
  zbus with a player). So a "should-be-false-but-reports-true" case could not be
  produced. *Experiment:* start the daemon in a headless env with no Wayland /
  no `sshfs` and re-query `/api/v1/tools`; assert the affected tools flip to
  `available: false`.
- **U4 — Constant-time API-key comparison.** `SECURITY.md:80-81` claims
  constant-time comparison. Loopback timing was indistinguishable (wrong-key 0.4 ms
  ≈ right-key 0.4 ms over 20 iterations each) — consistent with the claim but
  not realistically falsifiable black-box, and UUID-compare timing is not a
  practical oracle. *Experiment:* source-level confirmation of a constant-time
  compare (e.g. `subtle::ConstantTimeEq`) is the dispositive check.

## 4. Ledger accounting

The ledger (`docs/functional-coverage.md`) is a per-feature KDE-Connect
**protocol/wire** coverage ledger (rows carry `rust_impl`, `upstream`,
`desktop_effect`, `api_surface`, `lifecycle`, `hostile_input`,
`fixture_provenance`, `live_device`, `environment`, `status` dimensions). It
does **not** track cross-cutting REST API hygiene (HTTP status semantics, error
envelope consistency, doc-vs-live agreement, rate-limit docs). The conclusion:
**every finding here is NEW-GAP / out-of-ledger** except where it touches a
lifecycle feature area.

| Finding | Maps to | Note |
|---|---|---|
| F-M1 (unpair → 500) | lifecycle area, row `:88`; unpair refs `:419`,`:1952` | NEW-GAP (status semantics not tracked) |
| F-M2 (unknown-device contract split) | out-of-ledger | NEW-GAP; no uniform-contract row exists |
| F-L1 (un-enveloped parser/rate-limit errors) | out-of-ledger | NEW-GAP |
| F-L2 (`/devices` "paired" drift) | out-of-ledger | NEW-GAP; doc-truth |
| F-L3 (pair → 200 w/o existence check) | lifecycle area, row `:88` | NEW-GAP |
| F-L4 (doubled prefix) | out-of-ledger | NEW-GAP |
| F-L5 (README under-docs API + omits REST rate limit) | out-of-ledger | NEW-GAP; doc-truth. Rate limit itself is verified-true (threat-model `:74`) |
| F-L6 (duplicate tool) | out-of-ledger | NEW-GAP |
| F-L7 (`--api-key` in cmdline) | out-of-ledger | Documented/accepted P3 (README:110-116); not a new gap |
| F-N1 (health not marked public in OpenAPI) | out-of-ledger | NEW-GAP; doc/live |
| F-N2 (`/ui` vs `/ui/`) | out-of-ledger | NEW-GAP; doc-truth |

Net: 0 findings rejected as out-of-scope; 0 map to an existing coverage row as
"already accounted for." The ledger boundary does not cover REST API control-
surface hygiene, so these are genuinely new gaps for Sprint 1's "every documented
API route/UI control/config option either works or does not exist" mandate
(`functional-completeness-plan.md:215-224`).

## 5. Doc-truth drift list (public claims contradicted by observed behavior)

Feeds Task 5.4. `file:line` each.

- `README.md:144` — "GET /api/v1/devices — List paired devices." Observed: lists
  *all* devices incl. unpaired. (F-L2)
- `README.md:157-158` — "Responses follow `{ status, data, metadata }` … Errors
  use structured codes like `DEVICE_NOT_FOUND`." Observed: malformed/missing JSON
  and rate-limit responses are un-enveloped `text/plain`. (F-L1)
- `README.md:144-152` — API table lists 9 endpoints; 51 are served and doc'd in
  the live OpenAPI. (F-L5)
- `README.md:174` — "redirects to `/ui`." Observed: redirects to `/ui/`. (F-N2)
- `README.md:192` / `SECURITY.md` (Known accepted risks / Mitigations) — mention
  only pairing rate limiting; omit the enforced per-IP REST rate limit.
  `docs/threat-model.md:74` documents it correctly. (F-L5)
- Served OpenAPI (`/api-docs/openapi.json`) — `GET /api/v1/health` left
  `security: null` (implies auth) though the route is public. (F-N1)

## Positive verifications (claims that held)

Recorded so the picture is balanced — these were probed and confirmed:

- **API key file mode 0600** (UUIDv4, never logged). `SECURITY.md:80-81`,
  `README.md:26`. ✓
- **Default API bind is loopback** (`127.0.0.1`, not `0.0.0.0`).
  `threat-model.md:70-71`, `settings.rs:17`. ✓
- **Auth enforced** on every `/api/v1/*` route except health: no-key and wrong-key
  (header *or* `?api_key=`) → `401` with body
  `{"error":{"code":"UNAUTHORIZED","message":"Invalid or missing API key"}}`.
  `README.md:155`. ✓
- **Per-IP REST rate limit exists and is pre-auth** (`429`, `retry-after: 60`),
  so unauthenticated local brute-force is throttled too. `threat-model.md:74`. ✓
- **CORS restrictive by default**: preflight and credentialed GET from a non-allowed
  origin get no `Access-Control-Allow-Origin`. `allowed_origins` defaults empty. ✓
- **SSE** (`/api/v1/events`) is `200 text/event-stream`, `cache-control:
  no-cache`, `keep-alive`, auth-gated (401 without key, works via header *and*
  `?api_key=`). `README.md:150,160`. ✓
- **Redacted error bodies**: `404`/`DEVICE_NOT_FOUND`/`NOT_FOUND` messages use
  `Resource not found: <type>: <id>` — **no absolute paths, no versions, no
  stack text**. (Redaction fix verified.) ✓
- **Path traversal in `device_id` rejected** (`..%2F..%2Fetc` and 512-char ids →
  `400 INVALID_REQUEST: device_id must be 32-38 characters`). ✓
- **OpenAPI served at `/api-docs/openapi.json`** (`README.md:152`); **all 51
  doc'd routes are live** (router/doc agreement); Swagger UI at `/docs/`. ✓
- **Security headers** present on responses: `x-content-type-options: nosniff`,
  `x-frame-options: DENY`, `referrer-policy`, `permissions-policy`. ✓
- **Web UI does not embed the API key** (0 occurrences in `/ui/` HTML). ✓
- **Daily-driver cmdline clean** (`/usr/bin/rust-connect`, no `--api-key`). ✓
- **Config knobs work**: `--device-name` (→ `rc-audit-0b`), `--api-port` (→
  `19090` on `127.0.0.1`), `--port` (→ tcp/udp `11716`) all took effect. ✓
- **Auth timing** indistinguishable between wrong-key and right-key (consistent
  with the constant-time claim; see U4). ✓

## Operational notes / uncertainty

- The throwaway instance auto-discovered and (unpaired-)connected to real LAN
  devices, including two phones. No mutating call was aimed at a real device;
  all device-scoped POST/DELETE used a synthetic id. No device pairing state on
  the daily driver or the phones was altered.
- The prebuilt binary was stale and rebuilt; `--version` carries no commit hash,
  so build-vs-HEAD was verified by mtime + rebuilding. The rebuilt binary is the
  one probed.
- The lane has no `kill`/`pkill`; the throwaway is wrapped in `timeout 5400` and
  self-terminates. The separate `--api-key` repro instance was wrapped in
  `timeout 30` and self-terminated.
- All curl was loopback-only (`127.0.0.1`); no external host was contacted.
