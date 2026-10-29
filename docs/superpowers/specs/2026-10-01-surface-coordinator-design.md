# Surface Coordinator — Design

Date: 2026-10-01
Status: awaiting review

## Purpose

Remotely manage a fleet of Surface devices (10 to 500 per deployment) from a
manager application: change Windows settings, run scripts, launch applications,
and move devices between Wi-Fi networks. It must work in two situations:

- **No network available**: the manager PC opens a hotspot; devices find it,
  join it, and can be controlled and given real Wi-Fi credentials.
- **Normal network with internet**: devices are controlled through a central
  server, even when manager and devices are on different networks.

## Components

| Component | Runs on | Role |
|---|---|---|
| Coordinator | Linux server (primarily) | Central server. Clients connect to it; it stores state, dispatches jobs, serves the web UI. |
| Client | Managed device (Windows now) | Lightweight background service that executes jobs. |
| Manager | Operator PC (Windows first; macOS and Linux parity) | Desktop app used to control devices. |

The coordinator's web UI and the manager UI are the same frontend. The web UI
requires an account.

## Decisions

- **Stack**: Rust for client and coordinator; Tauri for the manager; one shared
  web frontend.
- **Hotspot mode**: the manager embeds the coordinator. Clients speak one
  protocol in both modes.
- **Trust**: an enrollment key supplied at client install. Client and
  coordinator mutually prove knowledge of it.
- **Managed devices**: Windows only for now. All OS-specific client code sits
  behind a platform interface so macOS and Linux clients can be added later
  without touching the protocol, coordinator or UI.
- **Client privilege**: runs as a Windows service under LocalSystem.

## Repository layout

One Cargo workspace plus one frontend package.

```
crates/
  protocol/          shared message types and (de)serialisation
  coordinator-core/  all server logic as a library
  coordinator/       server binary (thin wrapper around coordinator-core)
  client/            device agent
  manager/           Tauri app (embeds coordinator-core)
ui/                  web frontend, served by coordinator and bundled in manager
docs/
```

### Unit boundaries

- `protocol` depends on nothing in the workspace. Everything else depends on it.
- `coordinator-core` exposes: start a server on an address with a data
  directory and an auth mode; stop it. It knows nothing about Tauri or hotspots.
- `coordinator` parses configuration and calls `coordinator-core`.
- `manager` owns hotspot control and the choice between remote and embedded
  mode. It never talks to clients directly.
- `client` is split into a platform-independent core (connection, job
  scheduling, result reporting, network fallback state machine) and a
  `platform` module implementing a trait per OS.
- `ui` talks only to the coordinator HTTP/WebSocket API. It does not know
  whether the coordinator is remote or embedded, apart from a capability flag
  that shows hotspot controls when running inside the manager.

## Transport

### Client to coordinator

- The client opens one outbound WebSocket to the coordinator and keeps it open.
  No inbound ports on the device; works through NAT.
- Remote coordinator: `wss://` with normal certificate validation.
- Embedded coordinator on the hotspot: `ws://` to the gateway address on a
  fixed port. Confidentiality comes from the WPA2 hotspot passphrase;
  authenticity from the enrollment handshake below.
- Messages are JSON, each with a `type` tag and a protocol version in the
  handshake. Unknown message types are ignored, so older clients keep working.
- Heartbeat every 30 seconds. The coordinator marks a device offline after
  three missed heartbeats.
- Reconnect with exponential backoff and jitter, capped at 60 seconds.

### Handshake

1. Client sends `hello`: protocol version, device id (generated at install and
   persisted), hostname, OS, client version, enrollment key id, and a nonce.
2. Coordinator replies with `challenge`: its own nonce and an HMAC over both
   nonces using the enrollment key.
3. Client verifies the HMAC (proving the coordinator holds the key), then
   sends its own HMAC over both nonces.
4. Coordinator verifies and replies `welcome`. Either side closes the
   connection on failure.

The enrollment key never travels over the wire. A coordinator may hold several
enrollment keys; the key id selects one.

### UI to coordinator

- REST-style JSON over HTTP for reads and commands.
- One WebSocket for live updates: device status changes and job output.
- Remote mode: account login, session cookie, passwords hashed with Argon2id.
- Embedded mode: the coordinator binds to loopback for the UI and uses a
  random per-launch token handed to the UI by the manager. No account.

## Data model (SQLite)

- `accounts`: id, username, password hash, created.
- `enrollment_keys`: id, label, secret, created, revoked.
- `devices`: id, enrollment key id, hostname, OS, client version, last seen,
  online flag, current network (SSID, IP).
- `groups` and `group_members`.
- `scripts`: id, name, shell (PowerShell or cmd), body, built-in flag.
- `jobs`: id, kind, payload, created by, created.
- `job_targets`: job id, device id, state (queued, sent, running, succeeded,
  failed, expired), exit code, output, started, finished.

SQLite is sufficient at this scale: 500 devices heartbeating every 30 seconds
is about 17 writes per second at worst, and heartbeats only update memory
unless state changes.

## Jobs

A job is one action sent to one or more targets. Targets are selected devices,
a group, or all devices; the coordinator expands them into `job_targets` rows.

| Kind | Payload | Client behaviour |
|---|---|---|
| `script.run` | shell, body, timeout, run-as (system or logged-in user) | Runs the script, streams output, reports exit code. |
| `app.launch` | path, arguments | Starts the process in the logged-in user's interactive session. |
| `setting.apply` | setting id, value | Applies one entry from the built-in settings catalogue. |
| `wifi.join` | SSID, password, optional coordinator URL | Adds the profile and connects; see Wi-Fi handoff. |
| `power` | restart, shutdown or sleep | Commits once, asks Windows to perform the action, then reports its result. Restart/shutdown success means Windows accepted a five-second scheduled action; sleep finishes after resume. |

- Output is streamed to the coordinator in chunks and capped per target (1 MB);
  excess is truncated with a marker.
- Jobs for offline devices stay queued and are delivered on reconnect. Queued
  targets expire after a configurable period (default 24 hours).
- A client that receives a job kind it does not support reports `failed` with
  an `unsupported` reason. This is how future non-Windows clients degrade.

### Settings catalogue

A fixed list of named settings, each with a value type and a per-platform
implementation in the client. Initial Windows set: volume and mute, display
brightness, power and sleep timeouts, wallpaper, Windows Update pause,
taskbar and Start layout lock, time zone. Anything outside the catalogue is
done with a script.

## Client

- Windows service, LocalSystem, automatic start.
- Idle footprint target: under 10 MB RAM and no measurable CPU. One socket,
  one timer; no polling loops other than the network fallback below.
- Configuration file written by the installer: device id, enrollment key id and
  secret, coordinator URL (optional).
- Launching apps and running scripts as the logged-in user uses the active
  session's user token.

### Platform interface

One trait with methods for: run script, launch app in user session, apply
setting, list and join Wi-Fi networks, power action, and report system info.
`platform/windows` implements it now. Future platforms add a module; unsupported
operations return `unsupported`.

### Network state machine

1. **Connected**: WebSocket to the coordinator is up.
2. **Searching**: no coordinator reachable for 60 seconds. Every 30 seconds
   the client retries the configured coordinator and scans for the hotspot
   SSID.
3. **On hotspot**: hotspot SSID found; the client joins it and connects to the
   coordinator at the gateway address.

If no coordinator URL is configured, the client starts in Searching. The
client never abandons a working coordinator connection to join a hotspot.

## Hotspot mode

- SSID is `SC-` followed by the first eight characters of the enrollment key
  id. The passphrase is derived from the enrollment secret with HKDF, so
  clients know both without any exchange.
- The manager starts the hotspot and the embedded coordinator together and
  stops them together.
- Per manager OS:
  - Windows: Mobile Hotspot via the WinRT tethering API.
  - Linux: NetworkManager (`nmcli`).
  - macOS: no supported API exists. The manager shows the SSID and passphrase
    and instructions to enable Internet Sharing manually, then starts the
    embedded coordinator.

### Wi-Fi handoff

1. Operator sends `wifi.join` with SSID, password and, optionally, the remote
   coordinator URL to use from then on.
2. The client acknowledges, stores the profile and URL, and joins the network.
3. If it reaches a coordinator within 60 seconds, the job is reported as
   succeeded to whichever coordinator it reached.
4. Otherwise it returns to Searching, rejoins the hotspot if present, and
   reports the job as failed with the reason.

## Releases and self-update

The source lives in the public GitHub repository
`RealDoc06/surface-coordinator`. Releases are the single source of updates.

### Releases

- A version tag (`vX.Y.Z`) triggers a GitHub Actions workflow that builds the
  client (Windows), the manager (Windows, macOS, Linux) and the coordinator
  (Linux), and attaches them to a GitHub Release.
- All components share one version number.
- Every update artifact is signed in CI. The private keys are GitHub Actions
  secrets; the public keys are compiled into the client and manager. An
  artifact whose signature does not verify is never installed. This matters
  because the client runs as LocalSystem and the repository is public.

### Manager self-update

- Uses the Tauri updater against the repository's latest release.
- The manager checks at startup and on demand, shows the new version and
  release notes, and installs when the operator confirms.
- No update check is attempted without internet (hotspot mode); the manager
  keeps working on its current version.

### Client update

Client updates are started by the operator from the UI; clients never update
on their own.

- The coordinator checks GitHub for the latest release periodically and on
  demand, downloads the client artifact and its signature, and caches them in
  its data directory. The embedded coordinator does the same while the manager
  has internet, so a cached version can be pushed later on the hotspot.
- The device list shows each client's version and marks those older than the
  newest cached version.
- The operator updates selected devices, a group, or all, using a new job
  kind:

| Kind | Payload | Client behaviour |
|---|---|---|
| `client.update` | version, size, SHA-256 | Downloads, verifies, installs and restarts; see below. |

- The client always downloads from its coordinator
  (`/updates/client/<version>/<target>`), never from GitHub directly. One code
  path covers both internet and hotspot, and devices need no outbound access
  beyond the coordinator.

Install steps on the client:

1. Download to a temporary file next to the executable; check size and
   SHA-256; verify the signature against the built-in public key.
2. Rename the running executable to `client.old.exe`, move the new one into
   place, and write a pending-update marker with the previous version.
3. Report the job as `running` with a "restarting" note and exit; the service
   manager restarts the service on the new executable.
4. On start, the new version sees the marker. Once it completes a handshake
   with a coordinator it deletes the marker and the old executable and reports
   the job as succeeded with the new version.
5. If the new version starts three times without completing a handshake, it
   restores `client.old.exe`, restarts, and the old version reports the job as
   failed.

A client refuses to install a version older than its own unless the job sets
an explicit downgrade flag.

### Coordinator update

The remote coordinator is updated by the server administrator from the same
releases; it does not update itself. The embedded coordinator is updated with
the manager.

### Version compatibility

The handshake carries the protocol version. A coordinator accepts clients on
the current and previous protocol version, so a fleet can be updated after the
coordinator rather than at the same moment. A client on an unsupported
protocol version is still allowed to connect in a restricted state where the
only job it accepts is `client.update`.

## Embedded versus remote coordinator

They run the same code with separate databases. Data is not synchronised
between them. A device provisioned on the hotspot appears on the remote
coordinator the first time it connects there, identified by its device id and
enrollment key. The enrollment key must exist on both; the manager can export
and import keys.

## Error handling

- Connection loss on either side is normal operation: reconnect and resume.
  A job running on the client when the connection drops continues, and its
  result is sent after reconnect.
- Duplicate delivery is prevented by job-target ids; the client ignores a job
  it has already started or finished.
- Handshake failures are logged with the reason on both sides and surfaced in
  the UI as rejected connection attempts.
- The client logs to a rotating local file, capped in size.

## Testing

- `protocol`: round-trip serialisation tests and handshake HMAC vectors.
- `coordinator-core`: integration tests that start a server on a random port
  with a temporary database and drive it with a fake client.
- `client` core: tests against a fake platform implementation and a fake
  coordinator, covering the network state machine and job lifecycle.
- `platform/windows`: tested manually and with a small set of tests that run
  only on Windows.
- Hotspot and Wi-Fi handoff: manual test checklist on real hardware.
- `ui`: component tests for job creation and device selection.
- Client update: tests for signature and hash rejection, the downgrade guard,
  and the pending-marker rollback logic against a fake platform; the real
  executable swap is on the manual checklist.

## Milestones

Each milestone gets its own implementation plan.

1. **Core loop**: `protocol`, `coordinator-core`, `coordinator`, and `client`
   with handshake, heartbeat and `script.run`, proven end to end on localhost
   with the client run as a console process.
2. **UI and manager**: shared frontend, accounts, device list, job creation
   and live output; Tauri manager with the embedded coordinator.
3. **Hotspot**: hotspot control in the manager, client network state machine,
   `wifi.join` handoff.
4. **Full job set**: settings catalogue, `app.launch`, `power`, script
   library, groups.
5. **Packaging and updates**: client installer and service registration,
   coordinator deployment for Linux, manager installers for Windows, macOS and
   Linux, the signed release workflow, manager self-update and `client.update`.

## Out of scope

- Synchronising data between embedded and remote coordinators.
- Non-Windows clients (the design allows for them; none are built).
- Multi-tenant isolation; one coordinator serves one organisation.
- Roles and permissions; all accounts have full access.
- Remote screen viewing, file transfer and software inventory.
- Automatic (unattended) client updates and coordinator self-update.
