# Milestone 2 — UI and manager: implementation plan

Date: 2026-10-01
Spec: `docs/superpowers/specs/2026-10-01-surface-coordinator-design.md`
API contract: `docs/api.md`

## Goal

Shared frontend, accounts, device list, job creation and live output; Tauri
manager with the embedded coordinator.

Not in this milestone: hotspot control and the client network state machine
(milestone 3); groups, script library, settings, `app.launch`, `power`
(milestone 4); installers, service, releases and updates (milestone 5).

## Coordinator (`crates/coordinator-core`)

1. **Accounts and sessions.** Migration 2 adds `accounts` (id, username,
   password hash, created), `sessions` (token hash, account id or NULL,
   created, expires) and `rejected_connections`. Passwords hashed with
   Argon2id on the blocking pool. `AuthMode::Accounts` joins
   `AuthMode::Token`. Sessions last 7 days; the cookie holds a random token,
   the database its SHA-256.
2. **Auth layer.** A request is authenticated by a valid session cookie, or
   in token mode by the bearer token. Same-origin check on state-changing
   requests and on the events WebSocket. Failed logins are throttled per
   username in memory (5 failures → 30 s).
3. **API** as in `docs/api.md`: session, login, token exchange, logout,
   accounts, device detail and removal, job listing with paging, enrollment
   keys (create, import, export, revoke), rejected connections.
4. **Events.** A broadcast channel in the server state; every change to a
   device, job target or output publishes an event; `GET /api/v1/events`
   forwards them over a WebSocket and sends `resync` when a slow consumer
   lost events.
5. **Rejected handshakes** are stored (capped at 200 rows) as well as logged.
6. **UI serving.** `build.rs` embeds `ui/dist` into the library (a
   placeholder page if the UI has not been built), served with SPA fallback
   and cache headers. `Config.ui_dir` serves from disk instead, for
   development.
7. **Two listeners.** `Config.ui_bind`: when set, the API and UI are served
   only there and `bind` only accepts clients. The embedded coordinator uses
   loopback for the UI.
8. **Capabilities and extension routes.** `Config.extra_api_routes` lets the
   embedder (the manager) mount authenticated routes under `/api/v1`, and
   `Config.capabilities` is reported by `/session`. This is how hotspot
   control reaches the UI in milestone 3 without the UI talking to anything
   but the coordinator.

Binary: `coordinator account create|list|delete|set-password`, and
`serve --auth accounts|token` (accounts by default now), `--ui-dir`.

Tests (integration, as in milestone 1): login success and failure,
throttling, session cookie attributes, logout, token exchange, protected
routes in both modes, cross-origin refusal, accounts CRUD, keys CRUD with
import/export, device removal, rejected connections recorded, events for
connect/disconnect, job lifecycle and output (output events reassemble to the
stored output), resync, UI serving and fallback, split listeners.

## UI (`ui/`)

Vite + React + TypeScript, no runtime dependencies beyond React and a router,
no assets from the network (the UI must work on a hotspot with no internet).
Vitest and Testing Library for component tests.

Screens: login; devices (status, version, last seen, filter, selection);
run-script composer (shell, body, timeout, run-as, targets = selection or
all); jobs list; job detail with per-device state and live output; device
detail with its job history; enrollment keys; rejected connections; accounts.

Live updates through `/api/v1/events`, with refetch on `resync` and on
reconnect.

Tests: device selection and job creation components (spec), plus the event
reducer that keeps job output in step with stored output.

## Manager (`crates/manager`)

Tauri 2. The window first shows a small launcher bundled in the app: connect
to a remote coordinator (URL remembered) or start the local one. The local
one is `coordinator-core` in token mode with its database in the app data
directory, UI on a random loopback port, clients on a fixed port. The window
then navigates to the coordinator's own UI — remote or local — so the UI is
always same-origin with its API. Only the launcher page can call into the
manager; coordinator pages get no IPC.

`manager` is a workspace member but not a default member, so `cargo build`
and `cargo test` at the root do not need the desktop toolchain; build it with
`cargo build -p manager`.

Tests: URL validation and settings persistence; the embedded coordinator
start/stop path as an integration test without a window.

## Order of work

1. API contract (`docs/api.md`).
2. Coordinator: accounts, sessions, auth layer; then API additions; events;
   rejected connections; UI serving; listeners. Tests first.
3. UI.
4. Manager.
5. Demo: log in, see a device, run a script, watch output arrive; the same
   inside the manager with the embedded coordinator. README.

## Deviations

1. **Accounts are created from the CLI first** (`coordinator account
   create`), then in the UI. A "create the first account" web page would let
   whoever reaches a fresh coordinator first take it over.
2. **Token mode also uses the session cookie.** The spec says the embedded
   coordinator "uses a random per-launch token handed to the UI by the
   manager". The UI exchanges it once for a cookie, so both modes
   authenticate requests and the events WebSocket the same way.
3. **The manager navigates to the coordinator's UI** instead of rendering a
   bundled copy against a remote API. Same code, but always same-origin: no
   CORS, the session cookie works, and remote pages get no access to manager
   internals. The UI the manager bundles is the one inside the embedded
   coordinator.
4. **stdout and stderr stay interleaved** in one `output` column, as in the
   spec's data model.
