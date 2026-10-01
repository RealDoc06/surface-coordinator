# Surface Coordinator

Remotely manage fleets of Windows Surface devices: run scripts, change
settings, launch applications and move devices between Wi-Fi networks, with
or without an existing network.

- Design: [docs/superpowers/specs/2026-10-01-surface-coordinator-design.md](docs/superpowers/specs/2026-10-01-surface-coordinator-design.md)
- Milestone 1 plan and deviations from the design: [docs/superpowers/plans/2026-10-01-milestone-1-core-loop.md](docs/superpowers/plans/2026-10-01-milestone-1-core-loop.md)

## Status

Milestone 1 ("core loop") is implemented: a coordinator and a client that
authenticate each other with an enrollment key, heartbeats and online state,
and `script.run` jobs with streamed output, proven end to end on localhost.
There is no UI, no accounts, no manager app and no Windows service yet; the
client runs as a console process and the coordinator is driven through its
HTTP API.

## Layout

```
crates/protocol          message types, JSON codec, handshake HMAC helpers
crates/coordinator-core  all server logic as a library (the manager will embed it)
crates/coordinator       server binary, a thin wrapper around coordinator-core
crates/client            device agent: platform-independent core + platform/windows
scripts/demo-m1.ps1      the milestone 1 demo
```

## Build and test

Requires a stable Rust toolchain. SQLite is compiled in.

```
cargo build
cargo test
```

The tests that run real PowerShell and `cmd` scripts, and the end-to-end test
of client against coordinator, only run on Windows.

## Milestone 1 demo

On Windows, from the repository root:

```powershell
.\scripts\demo-m1.ps1
```

It builds the workspace, starts a coordinator on `127.0.0.1:18080`, enrolls
this machine as a device, runs a PowerShell job on it and prints the streamed
output and exit code, shows two clients with a wrong enrollment key being
rejected, and shows a job that was queued while the client was offline being
delivered when it reconnects. The enrollment key and API token are generated
on each run; state and logs go to `demo-data/`, which git ignores.

### The same by hand

Terminal 1, the coordinator:

```powershell
cargo build
.\target\debug\coordinator.exe key create --data-dir demo-data\coordinator --label demo
# prints "key id" and "key secret"; keep both for terminal 2

.\target\debug\coordinator.exe serve --bind 127.0.0.1:18080 --data-dir demo-data\coordinator
# prints "API token for this launch: ..."; keep it for terminal 3
```

Terminal 2, the client:

```powershell
$env:SC_KEY_SECRET = '<key secret>'
.\target\debug\client.exe --coordinator-url ws://127.0.0.1:18080 --key-id <key id> --state-dir demo-data\client
```

Terminal 3, the operator:

```powershell
$api = 'http://127.0.0.1:18080/api/v1'
$headers = @{ Authorization = 'Bearer <API token>' }

# The device is listed and online.
(Invoke-RestMethod "$api/devices" -Headers $headers).devices

# Run a script on every device.
$body = @{
    kind    = 'script.run'
    payload = @{ shell = 'powershell'; body = 'Write-Output "hello from $env:COMPUTERNAME"; exit 3' }
    targets = @{ type = 'all' }
} | ConvertTo-Json -Depth 5
$job = (Invoke-RestMethod "$api/jobs" -Method Post -Headers $headers -ContentType 'application/json' -Body $body).job

# State, exit code and output per device. Output grows while the job runs.
(Invoke-RestMethod "$api/jobs/$($job.id)" -Headers $headers).job.targets
```

- **Wrong key**: start a second client with a different `SC_KEY_SECRET` (and
  another `--state-dir`). It logs that the coordinator did not prove knowledge
  of the key and keeps retrying with backoff; the coordinator logs the failed
  handshake; the device never appears in `/devices`. With an unknown
  `--key-id` the coordinator rejects it with `unknown_key`.
- **Offline queue**: stop the client with Ctrl+C, create a job (its target
  stays `queued`), start the client again and read the job: it has run.

## HTTP API (milestone 1)

All routes except `health` need `Authorization: Bearer <token>`.

| Method and path | Purpose |
|---|---|
| `GET /api/v1/health` | Liveness and versions |
| `GET /api/v1/devices` | Devices with online state and last seen |
| `POST /api/v1/jobs` | Create a job: `kind`, `payload`, `targets` (`{"type":"all"}` or `{"type":"devices","device_ids":[...]}`) |
| `GET /api/v1/jobs` | Recent jobs with per-device state |
| `GET /api/v1/jobs/{id}` | One job with per-device state, exit code and output |

Timestamps are Unix milliseconds. Target states are `queued`, `sent`,
`running`, `succeeded`, `failed` and `expired`.

The coordinator serves plain HTTP and WebSocket. Bind it to loopback, as the
defaults do, or put it behind a TLS-terminating reverse proxy; clients then
connect with `wss://`.
