# Task 3.2 M4 brief — packaging, pinned-source KDE lane, ledger promotion (vk #991, M4 of 4 — final)

Read in order:
1. `plans/task-3.2-brief.md` — § M4 is your scope; § KDE reference two-phase
   pinning explains WHY this milestone exists.
2. `plans/task-3.2-m3-report.md` — the walls deferred to you.
3. `tests/interop/lib.sh` — the shared substrate (do not regress m1/m2/m3).

**Acceptance (plan § M4):** one-command runner for everything; the
pinned-SHA source-built KDE reference closes the "pinned KDE SHA"
criterion; docs (CI-vs-on-demand); ledger rows promoted with harness
evidence; vk #1018 (lock rewrite) rides this harness for live validation.

## Work items

### 1. Pinned-source KDE lane (the core item)

- Source tag `v26.04.3` (matches the distro NEVRA; do NOT blind-clone
  master — it's 26.11.70). The read-only clone at `/tmp/kdeconnect-kde`
  @ dcd6ded4 may be stale or gone — check; if gone, this lane MAY fetch
  the tag from invent.kde.org (explicit exception to the no-network
  fence, ONLY for this). `sudo dnf builddep kde-connect` needs root —
  likewise an explicit exception, ONLY for build deps; record every
  package installed.
- cmake build (~20 -devel packages, est. 5–15 min — UNMEASURED; measure
  and record). Artifacts cached under `tests/interop/.kde/` (gitignored
  — add the ignore entry).
- Selection via `RC_KDECONNECTD` env var: distro binary by default,
  source-built when set. lib.sh prints WHICH reference each run used
  (NEVRA vs `v26.04.3` source SHA).
- Re-run at least the M2 smoke against the source-built reference. If
  distro-binary vs source-built behave differently, that's a finding —
  record it.

### 2. M3's deferred walls — pick up what packaging unlocks

- **mpris**: build the zbus fake-player helper (pattern
  `tests/mpris_bus_recovery.rs:23-80`) as a small test binary or
  example; run the both-direction flow.
- **remotesystemvolume-out**: needs a pactl-subscribable daemon on the
  rust side. Try per-instance `pipewire-pulse` headless in the netns
  (it IS installed). Wall again = record again, with what was tried.
- **Spike A (XInput2 listener)**: `xinput`/`xev`/`xdotool` are absent.
  Do NOT install them. A tiny Python Xlib/XInput2 listener, or a rust
  example using the x11 crates already in Cargo.lock (check), is in
  scope if cheap; otherwise record.
- **clipboard kde→rust** needs a DISPLAY for the rust side — the
  harness can give the rust daemon its own Xvfb the same way kde gets
  one. That direction's wire already works; only the sink is missing.
- **runcommand stays fenced** (#1007 is George's ruling, not ours).

### 3. Packaging + docs

- `run.sh all` (or equivalent) running every milestone serially,
  visible-skip convention preserved.
- `tests/interop/README.md` updated: what runs where, root-only,
  CI-vs-on-demand stance (this is on-demand — netns needs root; say so
  plainly).
- Ledger: promote the desktop-peer `live_device`/environment cells that
  M1–M3 evidence now covers, per-row citations to the smoke + report.
  Rows still uncovered stay honestly UNVERIFIED — no blanket promotion.
  `docs/functional-coverage.md`; the ledger lint must stay green.

### 4. vk #1018 lock-rewrite validation

Read vk #1018 first (`~/bin/vk show 1018`). If its live-validation
shape fits this harness, add the minimal phase that exercises it. If it
needs the phone instead, say so in the report — do not force it.

## Standing discipline (unchanged)

- No `git push`, no merge, no `gh`. Commits on `task-3.2-m4`, real
  messages, commit as you go.
- Network fence exceptions: ONLY the invent.kde.org source fetch and
  `dnf builddep` for KDE build deps (item 1). Everything else stays
  netns+localhost. No `pass`. Writes only in this worktree,
  `/tmp/rc-m4-*`, and `tests/interop/.kde/`.
- sudo: harness scripts + the `dnf builddep` exception. Record usage.
- One cargo build at a time. Suite + clippy + fmt green; m1/m2/m3
  smokes stay green. Serial runs only. Zero-leak invariant.
- Upstream citations for behavioral claims; red-before-green for any
  new assertion family; instant-passing timeout assertions are broken.

## Deliverables

1. Source-built reference lane + `RC_KDECONNECTD` selection + cache dir.
2. Whatever of § 2 the packaging unlocks; walls re-recorded with
   attempts, not just restated.
3. `run.sh all` + README + ledger promotions (per-row citations).
4. #1018 verdict (exercised here, or explicitly phone-only).
5. `plans/task-3.2-m4-report.md` + everything committed on
   `task-3.2-m4`.
