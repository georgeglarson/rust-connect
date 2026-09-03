# Rust Connect — Constitution

## Founding Principles

### 1. AI Agents Are the Primary Users of the API

Rust Connect's consumers are LLMs, agents, and automated systems — not humans clicking buttons. Every API design decision serves this fact.

**API Contract:**
- **Predictable response envelope.** Every endpoint returns `{ "status", "data", "metadata" }`. No raw arrays. No varying shapes. Agents parse once, reuse everywhere.
- **Structured error codes.** Every error has a machine-readable `code` (`DEVICE_NOT_FOUND`, `PAIRING_PENDING`). Agents branch on codes, never string-match messages.
- **Idempotent operations.** Agents retry. `POST /devices/:id/pair` is safe to call twice. `DELETE` succeeds even if already deleted. Design for at-least-once semantics.
- **Stateless requests.** Every request is self-contained. No "call X before Y" unless enforced with clear error codes.
- **Complete OpenAPI spec.** `/docs` is the contract, not documentation. Every endpoint, parameter, response schema, error code. If it's not in the spec, it doesn't exist.
- **Events are first-class.** SSE at `/api/v1/events` is the primary push interface. Events are structured and include all context needed to act — no follow-up API calls required.
- **No human-only flows.** No "press a button to confirm." Flows requiring human intervention expose a monitorable state (`pairing_pending_verification`) that agents can poll or wait on via events.
- **Versioned API.** `/api/v1/` is explicit. Breaking changes increment the version.

**Code Implications:**
- The API layer is the product, not boilerplate.
- Response types are explicit structs, never `serde_json::Value`.
- Error codes are a controlled vocabulary defined in one place.
- Handlers are thin — they translate HTTP to service calls and format responses.

### 2. Single Responsibility Principle

SRP is the primary organizing principle. Every module, struct, and function has exactly one reason to change. This is not aesthetic — it is the mechanism that keeps the codebase navigable for AI agents that work on it.

**Why SRP matters for AI:** An LLM agent working on this codebase sees one file at a time with no ability to search across files. If a file has one responsibility, the agent can understand it fully, modify it correctly, and not break unrelated code. If a file has five responsibilities, the agent will miss one and introduce bugs.

**Rules:**

- **One file, one responsibility.** If a file's description needs "and," split it. A file that handles "connection setup and packet routing and TLS handshakes" is three files.
- **No god objects.** If a struct has > 10 fields, it's a dependency container, not a type. Question it aggressively.
- **No god functions.** If a function's description needs "and then," split it. A function that "connects and then pairs and then runs the packet loop" is three functions.
- **Layers don't skip.** `api` → `app` → `protocol`. Never `api` → `protocol` directly. Each layer translates between the layer above and the layer below.
- **Dependencies flow down.** Higher layers depend on lower layers. Lower layers never depend on higher layers. If `protocol` needs something from `api`, extract an interface in `protocol` that `api` implements.
- **No circular references.** If A imports B and B imports A, extract a shared interface or move the shared concept to a third module.
- **Files ≤ 500 lines** (production code, excluding tests) is the target, not an enforced gate. This is the natural result of SRP: a file past it usually has more than one responsibility. Known exceptions as of 2026-09-02: `api/handlers/device.rs` (~1100 lines) and `plugins/mpris/mod.rs` (~2900 lines); new code must not add to that list, and a change that touches one of them should split before it grows.
- **Functions ≤ 50 lines.** Same logic. A 200-line function is doing multiple things.
- **Max 3 levels of indentation.** Deep nesting means the function is tracking multiple concerns simultaneously. Extract.

**What SRP prevents:**
- God objects like `AppState` with 18 fields (it's a dependency container, not a state type)
- God files like `daemon.rs` at 666 lines (bootstrap + identity + service orchestration + connection lifecycle + signal handling)
- Handler files at 900+ lines (21 handlers each repeating auth boilerplate)
- Connection files at 1,500+ lines (TLS + TCP + keepalive + send/recv + cancel tokens + tests)

### 3. Security by Default

Security is not a sprint. It's the baseline.

**Rules:**
- **Validate at boundaries.** All input from network, filesystem, or API is untrusted.
- **Fail closed.** Errors deny access, not grant it.
- **No secrets in logs.** Ever.
- **Principle of least privilege.** Services run with minimum permissions.

---

## Enforcement

This file is consulted before every commit. If code violates a rule, it gets fixed — no exceptions, no "we'll do it later."

Violations found during code review become blocking issues, not nits.
