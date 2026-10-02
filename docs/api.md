# Coordinator HTTP API

The contract between the coordinator and the web UI. Everything is under
`/api/v1`. Bodies are JSON. Timestamps are Unix milliseconds. Errors are

```json
{"error": {"code": "machine_readable", "message": "for humans"}}
```

with a 4xx/5xx status. `401 unauthorized` means "not logged in" on any route
except the open ones listed under Authentication. `404 not_found` is a
missing resource or route, `403 cross_origin` a refused cross-origin request,
and `500 internal` an unexpected failure.

Contents: Authentication, Accounts, Devices, Groups, Jobs, Scripts, Settings
catalogue, Enrollment keys, Rejected connections, Updates, Hotspot (manager),
Live events, Downloads for clients, Other.

## Authentication

Two modes, fixed when the coordinator starts:

- `accounts` (remote coordinator): log in with username and password.
- `token` (coordinator embedded in the manager): a random per-launch token.

Either way the browser ends up with an `sc_session` cookie (HttpOnly,
SameSite=Strict, `Secure` when the coordinator is configured as reachable only
over HTTPS) and the UI never handles credentials again. A session lasts 7
days. Scripts and tools may instead send `Authorization: Bearer <token>` in
token mode; the header is ignored in accounts mode.

A session is bound to the mode it was created in. A cookie from an account
login is not accepted when the coordinator runs in token mode, and a cookie
from a token exchange is not accepted in accounts mode, even on the same data
directory. Sessions opened with a token are also ended whenever a token-mode
coordinator starts, because the token changes with every launch.

State-changing requests must be same-origin: if an `Origin` header is present
it has to match `Host`, otherwise `403 cross_origin`.

Failed logins are throttled per username only, not per address, and the
username is compared ignoring ASCII case. After 5 consecutive failures for a
username, further logins for it get `429 too_many_attempts` for 30 seconds.
A successful login clears the count.

| Route | Auth | Request | Response |
|---|---|---|---|
| `GET /session` | none | | `200` Session |
| `POST /auth/login` | none | `{username, password}` | `200 {account}` + cookie; `401 invalid_credentials`; `429 too_many_attempts`; `400 wrong_auth_mode` |
| `POST /auth/token` | none | `{token}` | `204` + cookie; `401 invalid_token`; `400 wrong_auth_mode` |
| `POST /auth/logout` | none | | `204`, cookie cleared |
| `GET /health` | none | | `200 {status: 'ok', version, protocol_version}` |

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
| `GET /accounts` | | `{accounts: Account[]}` ordered by username |
| `POST /accounts` | `{username, password}` | `201 {account}`; `400 invalid_username`, `400 weak_password`, `409 username_taken` |
| `POST /accounts/{id}/password` | `{password}` | `204`; `400 weak_password`; `404` |
| `DELETE /accounts/{id}` | | `204`; `400 cannot_delete_self`; `404` |

```ts
type Account = { id: string; username: string; created_at: number }
```

Usernames: 1 to 64 characters of `a-z A-Z 0-9 . _ -`, unique ignoring ASCII
case. Passwords: at least 10 characters and at most 1024 bytes. Setting a
password and deleting an account both end that account's sessions.

## Devices

| Route | Request | Response |
|---|---|---|
| `GET /devices` | | `{devices: Device[]}` ordered by hostname |
| `GET /devices/{id}` | | `{device: Device, targets: DeviceTarget[]}` (50 most recent); `404` |
| `DELETE /devices/{id}` | | `204`; forgets the device, its job history and its group memberships, closing its connection; jobs left with no target are removed; `404` |

```ts
type Device = {
  id: string
  hostname: string
  os: string
  client_version: string
  protocol_version: number       // of its last handshake
  enrollment_key_id: string
  online: boolean
  restricted: boolean            // see below; false while offline
  update_available: boolean      // client_version older than the newest cached release
  first_seen_at: number
  last_seen_at: number
  network: { ssid: string | null; ip: string | null }
  group_ids: string[]
}
type DeviceTarget = Target & { job: { id: string; kind: string; created_at: number } }
```

`online` reflects the live connection, and `last_seen_at` of an online device
is the time of its last message.

`network` is what the client reported in its last handshake (both null if it
reported nothing). It keeps its last reported value while the device is
offline.

`restricted` is true for a device connected with a protocol version this
coordinator no longer supports. It only receives `client.update` jobs; its
other jobs stay queued until it reconnects with a supported protocol version
(or expire, see Jobs). `update_available` is false when the device's
`client_version` does not parse as `major.minor.patch`.

## Groups

| Route | Request | Response |
|---|---|---|
| `GET /groups` | | `{groups: Group[]}` ordered by name |
| `POST /groups` | `{name}` | `201 {group}`; `400 invalid_name`; `409 name_taken` |
| `PATCH /groups/{id}` | `{name}` | `200 {group}`; `400 invalid_name`, `404`, `409 name_taken` |
| `DELETE /groups/{id}` | | `204`; members stay; `404` |
| `PUT /groups/{id}/members` | `{device_ids: string[]}` | `200 {group}`; replaces the membership; `400 unknown_device`; `404` |

```ts
type Group = { id: string; name: string; created_at: number; device_ids: string[] }
```

Names are 1 to 64 characters after trimming (a missing or non-string name is
`invalid_name`). They are unique ignoring case for ASCII letters only: `Lab`
and `lab` clash, `Ärzte` and `ärzte` do not. `PUT .../members` changes
nothing when any id is unknown, and repeated ids count once.

Group changes (create, rename, delete, membership, and forgetting a device
that was in a group) are announced with `{type: 'groups.changed'}`; refetch
groups and devices.

## Jobs

| Route | Request | Response |
|---|---|---|
| `POST /jobs` | CreateJob | `201 {job: Job}` (targets without output); `400 unsupported_kind`, `invalid_payload`, `unknown_device`, `unknown_group`, `no_targets` |
| `GET /jobs?limit=50&before=<created_at>` | | `{jobs: Job[]}` newest first, targets without `output`; `limit` 1 to 200 (default 50); `before` returns only jobs created earlier than that time |
| `GET /jobs/{id}` | | `{job: Job}` targets with `output`; `404` |
| `POST /jobs/{id}/cancel` | | `200 {job: Job}` (targets without output); `404` |

```ts
type CreateJob =
  | { kind: 'script.run'; payload: ScriptRun; targets: Targets }
  | { kind: 'wifi.join'; payload: WifiJoin; targets: Targets }
  | { kind: 'app.launch'; payload: AppLaunch; targets: Targets }
  | { kind: 'setting.apply'; payload: SettingApply; targets: Targets }
  | { kind: 'power'; payload: { action: 'restart' | 'shutdown' | 'sleep' }; targets: Targets }
  | { kind: 'client.update'; payload: { version: string; downgrade?: boolean }; targets: Targets }
type Targets =
  | { type: 'all' }
  | { type: 'devices'; device_ids: string[] }
  | { type: 'group'; group_id: string }      // members at the time of creation
type ScriptRun = {
  shell: 'powershell' | 'cmd'
  body: string                   // not empty after trimming
  timeout_secs?: number          // default 300, 1..86400
  run_as?: 'system' | 'user'     // default 'system'
}
type WifiJoin = {
  ssid: string                   // 1..32 bytes
  password?: string              // 8..63 characters; omit for an open network
  coordinator_url?: string       // ws:// or wss://, at most 2048 bytes; the coordinator to use from then on
}
type AppLaunch = { path: string; arguments?: string[] }
type SettingApply = { setting: string; value: boolean | number | string }
type Job = {
  id: string
  kind: string
  payload: unknown               // wifi.join: password shown as "********"
  created_by: string | null      // username, null in token mode, 'auto-update' for the update rollout
  created_at: number
  targets: Target[]
}
type Target = {
  id: string
  device_id: string
  state: 'queued' | 'sent' | 'running' | 'succeeded' | 'failed' | 'expired'
  exit_code: number | null
  reason: string | null          // see Failure reasons
  message: string | null
  output?: string                // stdout and stderr interleaved; absent in listings
  output_truncated: boolean
  created_at: number
  started_at: number | null
  finished_at: number | null
}
```

### Targets

`all` is every known device, online or not. `devices` with an unknown id is
`400 unknown_device` and repeated ids count once. `group` with an unknown
group is `400 unknown_group`. A job that would have no target (no devices, or
an empty group) is `400 no_targets`.

A target is `queued` until its device is online and the job is sent
(`sent`), then `running` once the device starts it, then `succeeded` or
`failed`. A target still queued 24 hours after creation becomes `expired`.

### Payloads

- `script.run`: `body` must not be empty after trimming.
- `wifi.join`: stored payloads returned by `GET /jobs` and `GET /jobs/{id}`
  have `password` replaced by `"********"` when present.
- `app.launch`: `path` 1 to 1024 characters and not blank after trimming (a
  path of only whitespace is `invalid_payload`), at most 64 arguments of at
  most 1024 characters each. NUL characters are refused.
- `setting.apply`: `setting` must be a catalogue id and `value` must satisfy
  its type (see Settings catalogue).
- `power`: `action` is `restart`, `shutdown` or `sleep`.
- `client.update`: see Updates.

All violations are `400 invalid_payload`. An unknown `kind` is
`400 unsupported_kind`. A client that does not know a kind reports its
targets as `failed` with reason `unsupported`.

Output is capped at 1 MiB per target. Beyond that it is cut and ends with
the marker `[output truncated]`, and `output_truncated` is true.

### Failure reasons

A failed target's `reason` is one of:

| Reason | Meaning |
|---|---|
| `unsupported` | The client cannot do this (unknown kind, setting or platform feature). |
| `timeout` | A `script.run` hit its timeout, or a `wifi.join` reached no coordinator on the new network in time. |
| `nonzero_exit` | A script exited with a non-zero `exit_code`. |
| `invalid_payload` | The client refused the payload. |
| `interrupted` | The client restarted while the job was running. |
| `error` | The job failed for another reason; see `message`. |
| `cancelled` | The job was cancelled. |
| `no_user_session` | The job needs a logged-in user (`run_as: 'user'`, user-scope settings, `app.launch`) and nobody is logged in on the device's console. |
| `bad_signature` | `client.update`: the artifact's signature does not verify. |
| `bad_hash` | `client.update`: the download's size or SHA-256 differs from the announced one. |
| `downgrade_refused` | `client.update`: the version is older than the running one and `downgrade` was not set. |
| `download_failed` | `client.update`: the artifact could not be fetched. |
| `rolled_back` | `client.update`: the new version never completed a handshake and the previous one was restored. |

### Wi-Fi handoff

A `wifi.join` target that is still `running` after its device has been
offline for the grace period (two minutes by default) becomes `succeeded`
with `reason: null` and a `message` saying the device left and did not return
(it is presumed to have joined the network and may now be reporting to
another coordinator). The presumption is not applied until the coordinator
itself has been running for the grace period, so a coordinator that has just
started gives devices a full period to reconnect.

### Cancel

`POST /jobs/{id}/cancel` acts on the unfinished targets of the job and leaves
finished ones untouched. Each change is announced with `job.target.updated`.

- `queued` targets become `failed` with reason `cancelled` at once.
- For `sent` and `running` targets the device is told to stop
  (`job.cancel`). The target becomes `failed`/`cancelled` when the device
  confirms, or at once if the device is offline.
- A device can only stop a job before its point of no return. After it, the
  cancel is ignored and the target ends with the job's own result, which may
  be `succeeded`. The point differs by kind: a script can be stopped at any
  time, along with its process tree; `app.launch` and `setting.apply` cannot
  be stopped once the process is started or the change begins; `power` cannot
  be stopped once the device has reported success; `wifi.join` cannot be
  stopped once the device has joined the network.
- A device that never received the job (its message was lost) reports the
  target as cancelled all the same, and will not run the job if it arrives
  later.
- When a device connects, the coordinator sends it `job.cancel` again for its
  targets cancelled in the last 24 hours, because a target cancelled while the
  device was offline may still be running there. The device ignores ids it
  does not know or has already finished.

## Scripts

The script library. There is no `GET /scripts/{id}`; `GET /scripts` returns
every script with its body. Script changes are not announced by events.

| Route | Request | Response |
|---|---|---|
| `GET /scripts` | | `{scripts: Script[]}` ordered by name, with bodies |
| `POST /scripts` | `{name, shell, body}` | `201 {script}`; `400 invalid_script`; `409 name_taken` |
| `PUT /scripts/{id}` | `{name, shell, body}` | `200 {script}`; `400 invalid_script`, `404`, `409 name_taken`; `403 built_in` for built-in scripts |
| `DELETE /scripts/{id}` | | `204`; `404`; `403 built_in` |

```ts
type Script = {
  id: string
  name: string
  shell: 'powershell' | 'cmd'
  body: string
  built_in: boolean
  created_at: number
  updated_at: number
}
```

`invalid_script` covers every problem with the body of the request, a missing
or mistyped field included: the name is 1 to 100 characters after trimming
(counted in characters), the shell is `powershell` or `cmd`, and the script
is not empty after trimming and at most 256 KiB (262144 bytes, counted in
bytes, not characters).

Names are unique ignoring case for ASCII letters only, as for groups.

Built-in scripts have fixed ids starting with `builtin-` and are read-only.
The coordinator stores them at start: it adds missing ones and refreshes the
name and body of existing ones. A built-in whose name a user script already
holds is skipped.

Jobs copy the script body. A job created from a library script does not
refer to it.

## Settings catalogue

`GET /settings-catalogue` → `{settings: Setting[]}`

```ts
type Setting = {
  id: string
  label: string
  description: string
  scope: 'machine' | 'user'           // user: needs someone logged in on the device
  value:
    | { type: 'bool' }
    | { type: 'integer'; min: number; max: number; unit: string }
    | { type: 'text'; max_len: number }
}
```

Values for `setting.apply`:

- `bool`: `true` or `false`.
- `integer`: a whole number within `min` and `max`. A non-integer such as
  `50.5` is refused, and so is `50.0`.
- `text`: not empty, at most `max_len` characters (counted in characters, not
  bytes), and without control characters.

The catalogue has these entries:

| id | value | scope |
|---|---|---|
| `audio.volume` | integer 0 to 100 (%) | user |
| `audio.mute` | bool | user |
| `display.brightness` | integer 0 to 100 (%) | machine |
| `power.display_timeout_ac`, `power.display_timeout_battery` | integer 0 to 600 (minutes, 0 means never) | machine |
| `power.sleep_timeout_ac`, `power.sleep_timeout_battery` | integer 0 to 600 (minutes, 0 means never) | machine |
| `desktop.wallpaper` | text, at most 260 characters: path of an image on the device | user |
| `windows_update.pause_days` | integer 0 to 35 (days, 0 resumes updates) | machine |
| `taskbar.locked` | bool | user |
| `start.layout_locked` | bool | user |
| `time.zone` | text, at most 64 characters: Windows time zone id | machine |

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

The label is trimmed and at most 100 characters. An imported `id` is 8 to 64
characters of `a-z A-Z 0-9 - _`, and the `secret` 32 to 256 printable ASCII
characters; giving only one of the two is `invalid_key`. Revoking a key does
not disconnect devices already connected; the key is refused at their next
handshake.

## Rejected connection attempts

`GET /rejected-connections` → `{rejected: Rejected[]}`, newest first, at most
200. Only attempts that got as far as a readable `hello` are recorded.

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

## Updates

The coordinator keeps a cache of verified client releases, read from the
latest release of a GitHub repository or uploaded by an operator, so that
devices can be updated with `client.update` jobs. A release is cached only
after its signature verifies against the update public key built into the
coordinator.

| Route | Request | Response |
|---|---|---|
| `GET /updates` | | `200 Updates` |
| `POST /updates/check` | | `200 Updates` after checking the release source now; `502 update_check_failed` (message says why; `Updates.last_error` is set too) |
| `POST /updates/upload?version=<x.y.z>&target=windows-x86_64[&notes=<text>]` | the executable, `application/octet-stream`, at most 200 MiB; header `X-Signature: <base64 signature>` | `201 Updates`; `400 invalid_version`, `400 unsupported_target`, `400 bad_signature`, `409 version_exists`, `413 too_large`, `503 updates_unavailable` |
| `POST /updates/auto` | `{enabled: boolean}` | `200 Updates` |

```ts
type Updates = {
  enabled: boolean               // an update key is configured: uploads are accepted
  checking: boolean              // the release source is checked (periodically and on request)
  auto_update: boolean           // the automatic rollout is on
  source: string | null          // e.g. "RealDoc06/surface-coordinator"; null while not checking
  last_checked_at: number | null
  last_error: string | null
  latest: ClientRelease | null   // newest cached, verified version
  cached: ClientRelease[]        // newest first
  rollout: Rollout | null        // null while auto_update is off or nothing is cached
}
type ClientRelease = {
  version: string                // "1.2.3"
  target: string                 // "windows-x86_64"
  size: number
  sha256: string
  cached_at: number
  notes: string | null
}
type Rollout = {
  version: string                // the newest cached release, L
  state: 'canary' | 'rolling' | 'halted' | 'done'
  canary_device_id: string | null   // device of the first automatic attempt for L
  updated: number                // devices reporting L, online or not
  pending: number                // devices older than L without a finished attempt for L
  failed: number                 // devices older than L whose latest attempt for L failed or expired
}
```

When checking is on, the coordinator checks at start and then every 6 hours
by default. After any check, successful or not, `last_checked_at` is set;
`last_error` holds the reason for a failed check and is cleared by a
successful one. Caching a new release, by a check or an upload, is followed
by a `resync` event, because every device's `update_available` may have
changed.

While checking is off (turned off, or no update key), `POST /updates/check`
returns `200` with `checking: false` and contacts nothing, and
`last_checked_at` stays null.

### Upload

`POST /updates/upload` adds a release without the release source, and works
whether or not checking is on. The body is the client executable; the
`X-Signature` header holds the base64 signature that the release tool makes
for the version, target and SHA-256 of the file (the `.sig` asset of a
release). The coordinator stores the body as it arrives, verifies the
signature, and only then caches it exactly as a release from the source:
same files, same download routes, same `ClientRelease`. `notes` is optional
and cut to 4000 characters; an empty one is `null`.

- `400 invalid_version`: `version` is missing or not `major.minor.patch`.
  Another spelling of a version is the same version (`01.2.3` is `1.2.3`).
- `400 unsupported_target`: `target` is missing or not `windows-x86_64`.
- `400 bad_signature`: `X-Signature` is missing, or the signature does not
  verify for this file, version and target. Nothing is kept.
- `409 version_exists`: that version is already cached for the target.
- `413 too_large`: the body is larger than 200 MiB (refused at once when
  `Content-Length` says so).
- `503 updates_unavailable`: no update key is configured.

### Automatic rollout

`auto_update` is stored by the coordinator; `POST /updates/auto` changes it
(the coordinator's start option only sets it the first time). While it is on,
the coordinator rolls out the newest cached release L in stages, from its
periodic sweep (every minute), right after a release is cached, when a device
comes online, and when `auto_update` is turned on:

- A device is outdated when it is online, its `client_version` is older than
  L, and it has no `client.update` target for L yet, in any state. So every
  device gets at most one automatic attempt per version; devices on L or
  newer, and versions that do not parse, are never targeted.
- Until some device has been updated to L (a succeeded `client.update`
  target for L that is not "already on version"), the lowest-id outdated
  device gets L alone: the canary. Nothing else happens while its target is
  unresolved.
- If an automatic target for L failed or expired before any device was
  updated, the rollout is halted. It resumes when an operator updates any
  device to L by hand with success.
- Once a device has been updated, every outdated device gets L in one job.
  Devices that come online later get it then.

Rollout jobs are ordinary `client.update` jobs (`downgrade: false`) with
`created_by: 'auto-update'`, announced and dispatched like any other.

`rollout.state` is `done` when `pending` is 0, else `halted` as above, else
`canary` while no device has been updated to L (also before a canary was
picked, when no outdated device is online), else `rolling`. A device whose
update succeeded counts as `updated` once it reconnects reporting L.

Cached releases are never pruned: there is no removal by age or count. (At
start, an entry whose files have gone missing is forgotten.)

`/updates` itself is a page of the web UI. The download routes below are
`/updates/client/...`.

### `client.update` jobs

`POST /jobs` accepts `kind: 'client.update'` with payload
`{ version: string; downgrade?: boolean }`. The version is `major.minor.patch`
and must be cached (`400 invalid_payload` otherwise). The coordinator fills in
the rest, so the payload dispatched to devices, and shown in the stored job, is

```ts
type ClientUpdate = {
  version: string
  target: string
  size: number
  sha256: string
  downgrade: boolean
}
```

Devices already on that version finish as `succeeded` with a message saying
so and are not sent anything. See Failure reasons for the `client.update`
reasons.

## Hotspot (only when `capabilities.hotspot`; served by the manager)

The manager adds these routes to its embedded coordinator, behind the same
authentication. Without the capability they do not exist (`404`).

| Route | Request | Response |
|---|---|---|
| `GET /manager/hotspot` | | `200 Hotspot` |
| `POST /manager/hotspot/start` | `{key_id}` | `200 Hotspot`; `404 not_found` (no such key); `400 key_revoked`; `400 bad_request` (malformed body); `502 hotspot_failed` (message says why) |
| `POST /manager/hotspot/stop` | | `200 Hotspot`; `502 hotspot_failed` |

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

Starting while the hotspot is on switches it to the given key. After a failed
start, `state` is `error` and `key_id`, `ssid` and `passphrase` are kept next
to `error`, because the hotspot may be half on and the operator still sees
what it was meant to be.

`stop` while the state is `off` returns `200` with state `off` and does
nothing, so a Mobile Hotspot the operator turned on for another purpose is
left alone.

With `control: 'manual'`, `start` records the key and returns `state: 'on'`
with the SSID, passphrase and instructions; the operator turns the hotspot on
themselves, and `stop` only forgets the recorded key. With
`control: 'unsupported'`, `start` fails with `hotspot_failed`. Devices
enrolled with the key find the SSID, join it and connect to the embedded
coordinator on `client_port`.

## Live events

`GET /events` upgrades to a WebSocket (same cookie; cross-origin upgrades are
refused with `403 cross_origin`). The server sends JSON text messages; the
client sends nothing.

```ts
type Event =
  | { type: 'device.updated'; device: Device }              // new, online/offline, changed
  | { type: 'device.removed'; device_id: string }
  | { type: 'job.created'; job: Job }                       // targets without output
  | { type: 'job.target.updated'; job_id: string; target: Target }   // without output
  | { type: 'job.output'; job_id: string; job_target_id: string; data: string; truncated: boolean; offset: number }
  | { type: 'connection.rejected'; rejected: Rejected }
  | { type: 'groups.changed' }                              // refetch groups and devices
  | { type: 'resync' }                                      // events were dropped: refetch
```

`job.output.data` is exactly what was appended to the target's stored
`output` (including the truncation marker when `truncated` is true), so a
view that loaded a job and then applies events in order stays identical to
the stored output.

`job.output.offset` is the length, in Unicode code points, of the target's
stored output before this event's `data` was appended. A view that loaded a
job with output of N code points ignores events with `offset < N` and appends
the rest.

Unknown event types must be ignored.

## Downloads for clients

Not under `/api`, no authentication, served where clients connect:

- `GET /updates/client/<version>/<target>`: the client executable
  (`application/octet-stream`).
- `GET /updates/client/<version>/<target>.sig`: the signature
  (`text/plain`). The body is the base64 signature followed by a newline.

Only cached releases are served, and only the canonical spelling of the
version: `1.2.3` works, `01.2.3` is `404`. The only target is
`windows-x86_64`. Anything else is `404 not_found`.

## Other

- Everything outside `/api`, `/client` and `/updates/client` serves the web
  UI, with unknown paths falling back to `index.html`. Methods other than
  `GET` and `HEAD` get `405`, and a mistyped `/api/...` route is
  `404 not_found`, not the UI.
- The client WebSocket is `/client/ws`. It is not part of this contract.
- When the coordinator is started with a separate UI address, the API, the
  web UI and `GET /health` are served only there, and the client address
  serves `/client/ws` and the downloads above.
