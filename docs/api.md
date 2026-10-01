# Coordinator HTTP API

The contract between the coordinator and the web UI. Everything is under
`/api/v1`. Bodies are JSON. Timestamps are Unix milliseconds. Errors are

```json
{"error": {"code": "machine_readable", "message": "for humans"}}
```

with a 4xx/5xx status. `401 unauthorized` means "not logged in" on any route.

## Authentication

Two modes, fixed when the coordinator starts:

- `accounts` (remote coordinator): log in with username and password.
- `token` (coordinator embedded in the manager): a random per-launch token.

Either way the browser ends up with an `sc_session` cookie (HttpOnly,
SameSite=Strict) and the UI never handles credentials again. Scripts and
tools may instead send `Authorization: Bearer <token>` in token mode.

State-changing requests must be same-origin: if an `Origin` header is present
it has to match `Host`, otherwise `403 cross_origin`.

| Route | Auth | Request | Response |
|---|---|---|---|
| `GET /session` | none | | `200` Session |
| `POST /auth/login` | none | `{username, password}` | `200 {account}` + cookie; `401 invalid_credentials`; `429 too_many_attempts`; `400 wrong_auth_mode` |
| `POST /auth/token` | none | `{token}` | `204` + cookie; `401 invalid_token`; `400 wrong_auth_mode` |
| `POST /auth/logout` | none | | `204`, cookie cleared |

```ts
type Session = {
  authenticated: boolean
  auth_mode: 'accounts' | 'token'
  account: { id: string; username: string } | null   // null in token mode
  capabilities: {
    accounts: boolean   // account management available (accounts mode)
    hotspot: boolean    // running inside the manager with hotspot control
  }
  version: string
  protocol_version: number
}
```

In token mode the manager opens the UI at `/#token=<token>`. The UI posts it
to `/auth/token` and removes the fragment.

## Accounts (accounts mode only; `404` in token mode)

| Route | Request | Response |
|---|---|---|
| `GET /accounts` | | `{accounts: Account[]}` |
| `POST /accounts` | `{username, password}` | `201 {account}`; `400 invalid_username`, `400 weak_password`, `409 username_taken` |
| `POST /accounts/{id}/password` | `{password}` | `204`; `400 weak_password`; `404` |
| `DELETE /accounts/{id}` | | `204`; `400 cannot_delete_self`; `404` |

```ts
type Account = { id: string; username: string; created_at: number }
```

Usernames: 1–64 characters of `a-z A-Z 0-9 . _ -`. Passwords: at least 10
characters.

## Devices

| Route | Request | Response |
|---|---|---|
| `GET /devices` | | `{devices: Device[]}` |
| `GET /devices/{id}` | | `{device: Device, targets: DeviceTarget[]}` (50 most recent); `404` |
| `DELETE /devices/{id}` | | `204`; forgets the device and its job history, closing its connection; `404` |

```ts
type Device = {
  id: string
  hostname: string
  os: string
  client_version: string
  enrollment_key_id: string
  online: boolean
  first_seen_at: number
  last_seen_at: number
}
type DeviceTarget = Target & { job: { id: string; kind: string; created_at: number } }
```

## Jobs

| Route | Request | Response |
|---|---|---|
| `POST /jobs` | CreateJob | `201 {job: Job}` (targets without output); `400 unsupported_kind`, `invalid_payload`, `unknown_device`, `no_targets` |
| `GET /jobs?limit=50&before=<created_at>` | | `{jobs: Job[]}` newest first, targets without `output` |
| `GET /jobs/{id}` | | `{job: Job}` targets with `output`; `404` |

```ts
type CreateJob = {
  kind: 'script.run'
  payload: ScriptRun
  targets: { type: 'all' } | { type: 'devices'; device_ids: string[] }
}
type ScriptRun = {
  shell: 'powershell' | 'cmd'
  body: string
  timeout_secs?: number          // default 300, 1..86400
  run_as?: 'system' | 'user'     // default 'system'
}
type Job = {
  id: string
  kind: string
  payload: unknown
  created_by: string | null      // username, or null in token mode
  created_at: number
  targets: Target[]
}
type Target = {
  id: string
  device_id: string
  state: 'queued' | 'sent' | 'running' | 'succeeded' | 'failed' | 'expired'
  exit_code: number | null
  reason: string | null          // unsupported, timeout, nonzero_exit, invalid_payload, interrupted, error
  message: string | null
  output?: string                // stdout and stderr interleaved; absent in listings
  output_truncated: boolean
  created_at: number
  started_at: number | null
  finished_at: number | null
}
```

## Enrollment keys

| Route | Request | Response |
|---|---|---|
| `GET /enrollment-keys` | | `{keys: Key[]}` |
| `POST /enrollment-keys` | `{label}` to generate, or `{label, id, secret}` to import | `201 {key: Key & {secret: string}}`; `400 invalid_key`; `409 key_exists` |
| `GET /enrollment-keys/{id}/secret` | | `{id, secret}` (export); `404` |
| `POST /enrollment-keys/{id}/revoke` | | `204`; `404` |

```ts
type Key = {
  id: string
  label: string
  created_at: number
  revoked_at: number | null
  device_count: number
}
```

## Rejected connection attempts

`GET /rejected-connections` → `{rejected: Rejected[]}`, newest first, at most 200.

```ts
type Rejected = {
  id: number
  at: number
  peer: string                   // remote address
  device_id: string | null
  hostname: string | null
  key_id: string | null
  reason: string                 // human-readable
}
```

## Live events

`GET /events` upgrades to a WebSocket (same cookie; cross-origin upgrades are
refused). The server sends JSON text messages; the client sends nothing.

```ts
type Event =
  | { type: 'device.updated'; device: Device }              // new, online/offline, changed
  | { type: 'device.removed'; device_id: string }
  | { type: 'job.created'; job: Job }                       // targets without output
  | { type: 'job.target.updated'; job_id: string; target: Target }   // without output
  | { type: 'job.output'; job_id: string; job_target_id: string; data: string; truncated: boolean }
  | { type: 'connection.rejected'; rejected: Rejected }
  | { type: 'resync' }                                      // events were dropped: refetch
```

`job.output.data` is exactly what was appended to the target's stored
`output` (including the truncation marker when `truncated` is true), so a
view that loaded a job and then applies events in order stays identical to
the stored output. Unknown event types must be ignored.

## Other

- `GET /health` (no auth): `{status: 'ok', version, protocol_version}`.
- Everything outside `/api` and `/client` serves the web UI, with unknown
  paths falling back to `index.html`.

## Milestone 3 additions

### Device network

`Device` gains `network: { ssid: string | null; ip: string | null }` — what
the client reported in its last handshake (both null if it reported nothing).

### `wifi.join` jobs

`POST /jobs` accepts `kind: 'wifi.join'` with payload

```ts
type WifiJoin = {
  ssid: string                 // 1..32 bytes
  password?: string            // 8..63 characters; omit for an open network
  coordinator_url?: string     // ws:// or wss://; the coordinator to use from then on
}
```

Violations are `400 invalid_payload`. Stored payloads returned by `GET /jobs`
and `GET /jobs/{id}` have `password` replaced by `"********"` when present.

A `wifi.join` target that is still `running` after its device has been
offline for two minutes becomes `succeeded` with `reason: null` and a
`message` saying the device left and did not return (it is presumed to have
joined the network and may now be reporting to another coordinator).

### `job.output` offset

`job.output` events gain `offset: number`: the length, in Unicode code
points, of the target's stored output before this event's `data` was
appended. A view that loaded a job with output of N code points ignores
events with `offset < N` and appends the rest.

### Hotspot (only when `capabilities.hotspot`; served by the manager)

| Route | Request | Response |
|---|---|---|
| `GET /manager/hotspot` | | `200 Hotspot` |
| `POST /manager/hotspot/start` | `{key_id}` | `200 Hotspot`; `404 not_found` (no such key); `400 key_revoked`; `502 hotspot_failed` (message says why) |
| `POST /manager/hotspot/stop` | | `200 Hotspot` |

```ts
type Hotspot = {
  control: 'automatic' | 'manual' | 'unsupported'  // manual: the operator enables it by hand (macOS)
  state: 'off' | 'starting' | 'on' | 'error'
  key_id: string | null        // enrollment key the hotspot was started for
  ssid: string | null          // "SC-" + first 8 characters of the key id
  passphrase: string | null    // derived from the key's secret
  client_port: number          // port devices connect to on this computer
  error: string | null         // why the last start failed
  instructions: string[]       // steps for the operator when control is 'manual'
}
```

With `control: 'manual'`, `start` records the key and returns `state: 'on'`
with the SSID, passphrase and instructions; the operator turns the hotspot on
themselves. Devices enrolled with that key find the SSID, join it and connect
to the embedded coordinator on `client_port`.
