# FINDINGS.md — tool catalogue onto the Plugin trait

Audit item 7, 2026-09-06. The agent-facing surface at `GET /api/v1/tools`
used to be a 9-arm `match` in `src/api/handlers/plugins/mod.rs` that
knew each plugin by name. It moves onto the `Plugin` trait itself so
each plugin owns its own catalogue entries.

## What changed

**Module structure**

- New `src/plugins/tool.rs` holds `Tool` and `ToolParameter`.
  `src/plugins/mod.rs` re-exports both so the public path is
  `crate::plugins::{Tool, ToolParameter}` — byte-identical to the
  previous in-handler definition; OpenAPI schema and `GET /api/v1/tools`
  JSON shape unchanged.

**Trait surface**

- `Plugin::tools(&self) -> Vec<Tool>` with default `vec![]`. Doc
  comment says a tool names an existing REST route. Plugins that
  don't expose a REST route don't override.

**Plugin overrides (9 of 25)**

- ping → `ping_device` (POST /api/v1/ping)
- battery → `get_battery`
- clipboard → `get_clipboard` (no params)
- mpris → `get_media`
- telephony → `get_telephony`
- notification → `get_notifications` (no params)
- systemvolume → `list_local_sinks` (no params)
- sftp → `browse_sftp`
- remotecommands → `get_remotecommands`

Each entry carries the same name, description, capability string,
endpoint, method, parameters, and `available: true` that the previous
hand-written match produced.

**API layer**

- `list_tools` no longer takes `&self` over plugin knowledge; it walks
  `plugin_registry.list()`, looks each plugin up, calls `tools()`,
  applies `is_backend_available`, then sorts and dedupes by name.
  `capability_to_tool` is deleted.

- `src/api/openapi.rs` switches the schema imports from
  `handlers::plugins::{Tool, ToolParameter}` to
  `crate::plugins::{Tool, ToolParameter}`. Generated OpenAPI spec is
  unchanged.

**Tests** (`tests/route_table_lint.rs`)

- `test_list_tools_yields_today_nine_names_sorted` — pins the union
  of `tools()` across the 9 plugins that contribute an entry today:
  `browse_sftp, get_battery, get_clipboard, get_media,
  get_notifications, get_remotecommands, get_telephony,
  list_local_sinks, ping_device`. Any change to that list (add /
  remove / rename / re-order) is a behavioural change to
  `GET /api/v1/tools`.

- `test_plugins_with_device_routes_advertise_tools_ratchet` — for
  every plugin that owns a route under
  `/api/v1/devices/{device_id}/<plugin-ish segment>`, count if its
  `tools()` is empty. Pin = 7 today (sms, share, lock,
  remotekeyboard, findmyphone, contacts, connectivity). Pin may only
  go down.

- `test_plugin_tools_default_is_empty` — red test using a stub
  `Plugin` impl that doesn't override `tools()`. Catches a regression
  where the default returns non-empty.

## How it was verified

- `cargo build --locked` — clean (compiles the lib + bin).
- `cargo test --lib --all-features --locked` — 1165/1165 pass.
- `cargo test --all-features --locked --test route_table_lint` —
  5/5 pass (the 3 new tests plus the 2 pre-existing parity lints).
- `cargo test --all-features --locked --test openapi_lint` —
  `test_every_schema_ref_in_the_spec_resolves` passes (the schema
  ref after the import switch still resolves).
- `cargo test --all-features --locked --test api_plugin_endpoints` —
  23/23 pass, including `test_list_tools_marks_degraded_backends`,
  which is the existing behavioural witness that `is_backend_available`
  is applied per entry.
- `cargo test --all-features --locked --test api_integration` —
  36/36 pass.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`
  — clean.
- `cargo fmt --check` — clean.

The full `cargo test --all-features --locked` (every test target in
one cargo invocation) was attempted but the linker crashed with `Bus
error` on the tmpfs-backed `/tmp` build volume; that's an
environmental issue (lld parallel mode + tmpfs disk quota on this
host), not a code defect. Each test target was run individually with
`RUSTFLAGS="-C link-arg=-Wl,--no-keep-memory"` and all targets of
interest are green.

## Critique — blunt

**The 9-arm match is gone. The hand-written list of plugin names was
the only thing tying it together; the new code walks the same set
through the registry, which is the same walk the request handlers
already do for everything else. The trait method is the right home.**
The win is correctness, not ergonomics — there is no longer a second
place to remember to update when a plugin's route changes.

**The nine-name pin is the strongest witness in the diff.** Without
it, the catalog could silently drop entries when a plugin renames its
tools, and the next caller would discover the absence at runtime.
With it, every change to the catalogue is a deliberate pin update in
the same change.

**The device-route ratchet is the weakest of the three tests and it
should be.** It counts "plugins that own a device route but contribute
no `tools()` entry." Today that's 7. The audit estimated 16; that was
the raw first-segment count from the router (which includes
non-plugin segments like `connect`, `pair`, `volume`). The ratchet is
restricted to actual plugin-named segments, which is what "advertise"
means — but that restriction is enforced by a hardcoded list of
plugins in the test body, not by data. If a future plugin lands with
a device route and is forgotten by this list, the ratchet stays
silent. Worth a follow-up: derive the ratchet list from the registry
itself, not from a hand-maintained set.

**`test_plugin_tools_default_is_empty` is a thin test.** It uses a
stub `Plugin` impl with a few hand-rolled `name` /
`incoming_capabilities` / `outgoing_capabilities` / `handle_packet`
methods. The default-empty assertion is correct, but the test will
break in any way a future signature change to the trait could break
it. The right fix if the trait ever grows is to centralise a "minimal
Plugin impl" helper and reuse it; today the duplication is small
enough to leave.

**The CHANGELOG bullet lives under `[Unreleased] → Changed`.** The
move is behaviour-preserving (JSON and OpenAPI are identical) but
shifts where the knowledge lives — that's the textbook "Changed"
case, not "Added" or "Fixed".

**What I did NOT do**, deliberately: didn't touch any other plugin
file (the 16 plugins without a `tools()` override keep their empty
default), didn't rename `is_backend_available`, didn't refactor the
plugin registry itself, didn't push or open a PR.
