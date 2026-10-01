# Milestone 1 — Core loop: implementation plan

Date: 2026-10-01
Spec: `docs/superpowers/specs/2026-10-01-surface-coordinator-design.md`

## Goal

`protocol`, `coordinator-core`, `coordinator` and `client` with handshake,
heartbeat and `script.run`, proven end to end on localhost with the client run
as a console process.

Not in this milestone (and not scaffolded): web UI, accounts and login, the
Tauri manager, hotspot mode and the client network state machine, `wifi.join`,
`app.launch`, `setting.apply`, `power`, groups, the script library, Windows
service installation, packaging, the release workflow and self-update.

## Workspace

```
Cargo.toml                 workspace, shared dependency versions
crates/protocol/           message types, JSON codec, protocol version, handshake HMAC
crates/coordinator-core/   library: start/stop a server; storage; client WS; jobs; HTTP API
crates/coordinator/        binary: parse configuration, call coordinator-core
crates/client/             library (platform-independent core + platform module) and binary
```

Main dependencies: tokio, axum (HTTP + server WebSocket), tokio-tungstenite
(client WebSocket and test fakes), rusqlite with bundled SQLite, serde /
serde_json, hmac + sha2, rand, uuid, clap, tracing. `windows-sys` only for the
Windows platform module.

## Protocol (`crates/protocol`)

Every message is a JSON object with a `type` tag. Two enums, one per
direction. Both have a catch-all `Unknown` variant so a message with an
unrecognised `type` decodes successfully and is ignored by the receiver.
Unknown fields are ignored everywhere.

Client to coordinator:

| type | fields |
|---|---|
| `hello` | `protocol_version`, `device_id`, `hostname`, `os`, `client_version`, `key_id`, `nonce` |
| `auth` | `mac` |
| `heartbeat` | — |
| `job.started` | `job_target_id` |
| `job.output` | `job_target_id`, `seq` (from 1), `stream` (`stdout`/`stderr`), `data` |
| `job.result` | `job_target_id`, `status` (`succeeded`/`failed`), `exit_code?`, `reason?`, `message?` |

Coordinator to client:

| type | fields |
|---|---|
| `challenge` | `nonce`, `mac` |
| `welcome` | `heartbeat_interval_ms` |
| `reject` | `reason` |
| `heartbeat_ack` | — |
| `job` | `job_target_id`, `job_id`, `kind` (free string), `payload` (raw JSON) |
| `job.result_ack` | `job_target_id` |

`job.kind` is a plain string and `payload` raw JSON, so job kinds added in
later milestones need no protocol change; a client that does not know a kind
reports `failed` with reason `unsupported`.

`script.run` payload: `shell` (`powershell`/`cmd`), `body`, `timeout_secs`
(default 300), `run_as` (`system`/`user`, default `system`).

Failure reasons are short machine-readable strings: `unsupported`, `timeout`,
`nonzero_exit`, `invalid_payload`, `interrupted`, `error`.

Handshake helpers: nonce generation (32 random bytes, hex), MAC computation
and constant-time verification (HMAC-SHA-256, hex). See Deviations for the
exact MAC input.

Constants: `PROTOCOL_VERSION = 1`, `MAX_OUTPUT_BYTES = 1 MiB`,
`DEFAULT_HEARTBEAT_INTERVAL_SECS = 30`, `MISSED_HEARTBEATS_BEFORE_OFFLINE = 3`.

Tests (written first): round trip of every message, exact wire shape of
representative messages, unknown `type` and unknown fields tolerated, unknown
job kind decodes, HMAC vectors computed independently (Node `crypto`), MAC
rejects wrong key / swapped role / tampered nonce.

## Coordinator (`crates/coordinator-core`)

Public surface:

```rust
pub struct Config { bind, data_dir, auth, heartbeat_interval, queued_expiry, expiry_sweep_interval }
pub enum AuthMode { Token(String) }
pub async fn start(config) -> Result<Coordinator>
impl Coordinator { fn local_addr(); async fn stop(self) }
pub mod admin { create_enrollment_key, list_enrollment_keys, revoke_enrollment_key }  // by data dir
```

### Storage

SQLite file `coordinator.db` in the data directory, WAL mode, schema versioned
with `PRAGMA user_version` and an ordered list of migrations. One connection
behind a mutex, used through `spawn_blocking`. Timestamps are Unix
milliseconds.

Tables created now: `enrollment_keys`, `devices`, `jobs`, `job_targets`
(spec columns plus `reason`, `message`, `output_truncated`, `last_seq`,
`created`). `accounts`, `groups`, `scripts` and device network columns are
left to the migrations of the milestones that use them.

### Client connections (`GET /client/ws`)

1. Read `hello` (10 s handshake timeout). Reject unsupported protocol
   version, unknown or revoked key.
2. Send `challenge`; read `auth`; verify. On failure send `reject` and close.
3. Upsert the device, register the connection (replacing an older connection
   of the same device id), mark online, send `welcome`.
4. Dispatch: re-send targets in `sent`/`running`, send `queued` targets and
   mark them `sent`.
5. Loop: any inbound message refreshes `last_seen` (memory only). No message
   for 3 heartbeat intervals → close and mark offline. The database is only
   written on online/offline transitions.

Job messages from a client are accepted only for targets that belong to that
device. `job.started`: `sent` → `running`. `job.output`: appended if `seq` is
newer than the last stored one, capped at 1 MiB with a truncation marker.
`job.result`: moves a non-terminal target to `succeeded`/`failed`; always
acknowledged with `job.result_ack` (idempotent).

A background sweep marks `queued` targets older than the expiry period
(default 24 h) as `expired`. On startup all devices are marked offline.

### HTTP API (`/api/v1`, bearer token)

| Method and path | Purpose |
|---|---|
| `GET /api/v1/health` | Liveness, versions. No auth. |
| `GET /api/v1/devices` | List devices with online state. |
| `POST /api/v1/jobs` | Create a job: `kind`, `payload`, `targets` (`{"type":"all"}` or `{"type":"devices","device_ids":[…]}`). |
| `GET /api/v1/jobs` | List jobs with target states (no output). |
| `GET /api/v1/jobs/{id}` | One job with per-target state, exit code, output. |

Errors are `{"error":{"code","message"}}`. Target selection is a tagged
object so `group` can be added in milestone 4.

Tests (integration, random port, temporary data dir, fake client built on
tokio-tungstenite): handshake success and every rejection path, online/offline
on connect/disconnect, heartbeat timeout, job dispatch and full lifecycle,
`all` and selected targets, validation errors, offline queue delivered on
connect, re-send of unresolved targets on reconnect, duplicate output ignored,
output cap, results from the wrong device ignored, expiry, API auth,
unknown messages ignored, connection replacement, persistence across restart.

## Coordinator binary (`crates/coordinator`)

`coordinator serve --bind 127.0.0.1:8080 --data-dir DIR [--api-token T]`
(token also from `SC_API_TOKEN`; generated and printed when absent) and
`coordinator key create|list|revoke --data-dir DIR`.

## Client (`crates/client`)

```
src/lib.rs            Client::run(shutdown)
src/config.rs         ClientConfig; device id generated once and persisted in the state dir
src/backoff.rs        exponential backoff with jitter, capped at 60 s
src/ledger.rs         persisted record of started/finished job targets
src/jobs.rs           job queue, lifecycle, output retention, result reporting
src/connection.rs     connect, handshake, session loop, heartbeat
src/platform/mod.rs   Platform trait and types
src/platform/windows.rs
src/main.rs           console binary
```

Platform trait (only what this milestone uses; later milestones add methods
with a default body returning `Unsupported`):

```rust
trait Platform {
    fn system_info(&self) -> SystemInfo;
    fn run_script(&self, request: ScriptRequest, output: mpsc::Sender<OutputChunk>)
        -> impl Future<Output = Result<ScriptOutcome, PlatformError>> + Send;
}
```

Windows `run_script`: writes the body to a temporary file, runs
`powershell.exe` or `cmd.exe` with piped output, puts the process in a Job
Object so a timeout kills the whole process tree, decodes output as UTF-8
incrementally.

Core behaviour:

- Connect, handshake (client verifies the coordinator's MAC before sending
  its own), then a session loop. On any failure: back off and reconnect.
  Backoff resets after a `welcome`.
- Heartbeat at the interval the coordinator announces in `welcome`; no
  inbound message for 3 intervals → reconnect.
- Jobs run one at a time in arrival order (see Deviations).
- Duplicate protection: a job target id that is queued, running or finished
  is ignored. Started ids are written to the ledger file before execution, so
  a restarted client reports an in-flight job as `failed`/`interrupted`
  instead of running it twice.
- A running job survives connection loss. `job.started`, output (up to the
  1 MiB cap) and the result are retained until the coordinator acknowledges
  the result, and are replayed after each reconnect; the coordinator drops
  output chunks it already has by `seq`.

Tests (written first): backoff bounds and cap; ledger persistence and
interruption; against a fake coordinator and fake platform: hello contents,
MAC verification both ways, heartbeat, job lifecycle, non-zero exit, timeout,
unsupported kind, invalid payload, duplicate job, sequential order, reconnect
after drop, result delivered after reconnect, replay until acknowledged,
unknown messages ignored. Windows-only tests run real PowerShell and cmd.

## Order of work

1. Workspace skeleton and `protocol` (tests first).
2. `coordinator-core` storage, then client connections, jobs and HTTP API
   (integration tests first).
3. `coordinator` binary.
4. `client` core (tests first with fakes), then the Windows platform and binary.
5. End-to-end demo on localhost; README.

## Deviations

Decisions taken where the spec is ambiguous, silent or wrong. Added to as the
work proceeds.

1. **Handshake MAC input is role-labelled.** The spec says both sides send
   "an HMAC over both nonces". If both MACs covered the same bytes, a client
   without the key could echo the coordinator's MAC back and be accepted. The
   MAC input is therefore
   `"surface-coordinator/handshake/v1" 0x00 role 0x00 client_nonce 0x00 server_nonce`
   with `role` being `coordinator` or `client`. Still "an HMAC over both
   nonces using the enrollment key", but the two are not interchangeable.
2. **Explicit `auth`, `reject`, `heartbeat_ack` and `job.result_ack`
   messages.** The spec names `hello`, `challenge` and `welcome` only. `auth`
   is step 3 of the handshake. `reject` carries the reason before the
   coordinator closes (the spec wants reasons logged on both sides).
   `heartbeat_ack` lets the client detect a dead coordinator through NAT.
   `job.result_ack` is what makes "its result is sent after reconnect"
   reliable: the client retains a result until it is acknowledged.
3. **Auth mode in this milestone is a bearer token only.** Accounts are
   milestone 2. `AuthMode::Token` is the embedded-mode mechanism from the
   spec; the server binary uses it too until accounts exist, generating a
   random token per launch unless one is configured. The API is never
   unauthenticated, because it can run scripts on devices.
4. **Unsupported protocol versions are rejected.** The spec's restricted
   state exists only to deliver `client.update` (milestone 5). Until that job
   kind exists a restricted connection could do nothing, so the coordinator
   rejects with `unsupported_protocol`. Only coordinator behaviour changes
   when the restricted state is added; the wire format does not.
5. **`run_as: user` is reported as `unsupported`.** Running as the logged-in
   user needs the active session's token, which requires the LocalSystem
   service (milestone 5). `system` means "as the client process's own
   identity", which is LocalSystem once the client is a service.
6. **Jobs run one at a time per device, in order.** The spec does not say.
   Queued jobs delivered together after a reconnect would otherwise run
   concurrently, and operators will expect "install, then configure" to
   happen in that order. Each job is bounded by its timeout.
7. **One listener serves both the client WebSocket and the HTTP API.** The
   spec's embedded mode binds the UI to loopback while clients arrive on the
   hotspot interface; that split is left to milestone 2 with the manager. The
   two routers are built separately so they can be bound separately.
8. **Timestamps are Unix milliseconds** in the database and the API.
9. **Extra `job_targets` columns**: `reason`, `message`, `output_truncated`,
   `last_seq`, `created`. Needed for failure reasons, the truncation marker,
   duplicate-output detection and expiry.
10. **Enrollment keys are managed from the coordinator CLI** (and the
    `admin` functions in the library). No HTTP endpoint yet; that belongs
    with the milestone 2 UI.
