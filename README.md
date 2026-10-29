# Surface Coordinator

Remotely manage fleets of Windows Surface devices: run scripts, change
settings, launch applications, restart or shut down, and move devices between
Wi-Fi networks, with or without an existing network.

There are three components:

| Component | Runs on | What it does |
|---|---|---|
| **Coordinator** (`crates/coordinator`, `crates/coordinator-core`) | A Linux server, or inside the manager | Devices connect to it. It stores state in SQLite, dispatches jobs, and serves the web UI and the HTTP API. |
| **Client** (`crates/client`) | Each managed Windows device | A small agent, normally a Windows service running as LocalSystem. It runs the jobs the coordinator sends. |
| **Manager** (`crates/manager`) | The operator's PC | A desktop app (Tauri) that shows the coordinator's web UI: either a remote coordinator, or one it runs itself together with a Wi-Fi hotspot for places without a network. |

The web UI (`ui/`, React and Vite) is built into the coordinator binary, so the
coordinator and the manager show the same UI.

- Start here: [usage instructions](docs/usage.md)
- Release validation and screenshots: [0.2.4 validation](docs/validation-0.2.4.md)
- Design: [docs/superpowers/specs/2026-10-01-surface-coordinator-design.md](docs/superpowers/specs/2026-10-01-surface-coordinator-design.md)
- HTTP API: [docs/api.md](docs/api.md)
- Plans, with the deviations from the design: [milestone 1](docs/superpowers/plans/2026-10-01-milestone-1-core-loop.md),
  [2](docs/superpowers/plans/2026-10-01-milestone-2-ui-and-manager.md),
  [3](docs/superpowers/plans/2026-10-01-milestone-3-hotspot.md),
  [4](docs/superpowers/plans/2026-10-01-milestone-4-full-job-set.md),
  [5](docs/superpowers/plans/2026-10-01-milestone-5-packaging-and-updates.md)
- Checks that need real hardware: [docs/manual-tests.md](docs/manual-tests.md)

## Status

Version 0.2.4 fixes selected targets being lost when opening library scripts,
prevents pending dialogs from hiding unfinished writes, restores CMD scripts in
installed clients, and reports native Windows power failures in job results.
Sleep jobs remain running until resume. The earlier hardening remains: active Wi-Fi is preserved
during coordinator outages, reconnects and cancellation are resilient, hotspot
starts are idempotent, the UI recovers promptly, and release builds have test
gates. Fleet dispatch uses bounded notifications and durable grouped database
commits. Streamed output is appended in storage, and the UI loads only the
selected device's output. Standalone Windows binaries include their C runtime.
Service updates validate the candidate before replacement and roll
back if the new client cannot confirm its connection. See [validation evidence](docs/validation-0.2.4.md) for the exact checks
performed and the remaining hardware and release limitations.

The networking fix must be installed on each Surface; updating the coordinator
alone cannot change the behavior of an older client. Existing manually selected
Wi-Fi is now preserved even if Windows hides its SSID. Hotspot discovery runs
when Wi-Fi is disconnected; explicit Wi-Fi jobs can still move a device.

Upgrade the coordinator against a backed-up database: this release adds persistent
cancellation state, query indexes and append-only storage for active output.
For rollback, restore the matching backup
and previous binary together. See the [deployment guide](deploy/README.md).

## Repository layout

```
crates/protocol          message types, JSON codec, handshake and hotspot helpers
crates/coordinator-core  all server logic as a library (the manager embeds it)
crates/coordinator       server binary, a thin wrapper around coordinator-core
crates/client            device agent: platform-independent core + platform/windows
crates/manager           desktop app (Tauri), not a default workspace member
crates/release-tool      generates keys, signs and verifies client update files
ui/                      the web UI (built into the coordinator)
deploy/                  Linux deployment files (see deploy/README.md)
release-keys/            the client update public key and signing instructions
scripts/                 the demo script and helpers
docs/                    API reference, manual checklist, design and plans
```

## Prerequisites and build

- A stable Rust toolchain (the MSVC toolchain on Windows). SQLite is compiled in.
- Node.js and npm for the web UI.
- For the manager on Windows, WebView2 (part of Windows 11).

Build the web UI first, because the coordinator embeds `ui/dist` when it is
compiled (without it the coordinator serves a placeholder page):

```
cd ui
npm ci
npm run build
cd ..
```

Then the Rust workspace, and the manager separately, because it needs the
desktop toolchain:

```
cargo build
cargo build -p manager
```

The binaries are in `target\debug\`: `coordinator.exe`, `client.exe`,
`release-tool.exe` and `manager.exe`.

Development and test builds retain file/line backtraces but omit full debugger
symbols and incremental caches to keep disk usage down. Compiled dependencies
are still reused; rebuilding changed code can take longer. For a build with
full debugger symbols, use `cargo --config 'profile.dev.debug=2' build` (add
`-p manager` for the manager).

Tests:

```
cd ui && npm test && cd ..
cargo test
cargo test -p manager
```

Tests that run real PowerShell and `cmd` scripts only run on Windows.
Installers for the manager are described in
[crates/manager/README.md](crates/manager/README.md).

## Quick start on one machine

In PowerShell, from the repository root, after building. Everything lives in
`demo-data\`, which git ignores.

1. Create an account for the web UI (it asks for the password) and an
   enrollment key. The key command prints the key id and the secret, once:

   ```powershell
   .\target\debug\coordinator.exe account create --data-dir demo-data\coordinator admin
   .\target\debug\coordinator.exe key create --data-dir demo-data\coordinator --label demo
   ```

2. Start the coordinator. It listens on `127.0.0.1:8080` for the clients, the
   web UI and the API (`--bind` changes that):

   ```powershell
   .\target\debug\coordinator.exe serve --data-dir demo-data\coordinator
   ```

3. In a second terminal, run the client in the console with the key from step 1:

   ```powershell
   $env:SC_KEY_SECRET = '<key secret>'
   .\target\debug\client.exe run --no-hotspot-discovery --coordinator-url ws://127.0.0.1:8080 --key-id <key id> --state-dir demo-data\client
   ```

4. Open <http://127.0.0.1:8080>, sign in, and the device is listed as online.
   Select it and choose **Run script...**.

The client started this way runs as you, not as LocalSystem, so "run as
system" means "as this user". Console mode is for trying things out; use the
service below on real devices. `coordinator serve --help` and `client run
--help` list every option; most also read an environment variable
(`SC_BIND`, `SC_COORDINATOR_URL`, `SC_KEY_ID`, `SC_KEY_SECRET`, ...).

## Installing the client as a service

On a device, from an **elevated** prompt. The client installs itself; there is
no separate installer. The enrollment secret is read from `SC_KEY_SECRET`:

```powershell
$env:SC_KEY_SECRET = '<key secret>'
.\client.exe install --key-id <key id> --coordinator-url wss://coordinator.example.org
```

`install` copies the executable to `%ProgramFiles%\Surface Coordinator`, writes
the configuration to `%ProgramData%\Surface Coordinator` (readable only by
SYSTEM and Administrators, since it holds the secret), registers the service
`SurfaceCoordinatorClient` (automatic start, LocalSystem, restart on failure)
and starts it. Without `--coordinator-url` the client looks for the manager's
hotspot. `--install-dir` and `--state-dir` change the two locations. Running
`install` again replaces the executable and keeps the device id.

```powershell
.\client.exe uninstall            # stop and remove the service and the installed copy
.\client.exe uninstall --purge    # and also delete the configuration, device id and log
```

The service writes `client.log` in the state directory (rotated at 5 MiB).
Installation, service execution, signed updates and rollback are verified in a
clean Windows 11 VM. Physical Surface Wi-Fi checks and the exact validation
scope are recorded in [the release report](docs/validation-0.2.3.md).

## The manager

Run `target\debug\manager.exe` (or an installed build). The launcher offers two
choices:

- **Remote coordinator**: enter the address (`https://coordinator.example.org`)
  and Connect. The manager checks that a coordinator answers there and shows its
  web UI in the window; you log in with an account. **Coordinator > Change
  coordinator...** in the app menu returns to the launcher.
- **This computer**: **Start local coordinator** runs a coordinator inside the
  app, with its data in the app's data directory. Clients connect to this
  computer on port 47800. The UI opens already signed in (a per-launch token,
  no accounts) and has a **Hotspot** page. Stopping it, or quitting the manager,
  stops the coordinator and the hotspot.

Hotspot flow (for places with no network): create an enrollment key under
**Enrollment keys**, open **Hotspot**, pick the key and **Start hotspot**. The SSID is
`SC-` plus the start of the key id and the passphrase is derived from the key
secret; both are shown. On Windows and on Linux with NetworkManager the manager
turns the hotspot on itself; on macOS it shows the steps for Internet Sharing.
Devices enrolled with that key and disconnected from Wi-Fi search for the
hotspot and appear in the device list. Devices already on another Wi-Fi stay
there, even while the coordinator is unavailable.

Wi-Fi handoff: select the devices and choose **Move to Wi-Fi...** with the
SSID, the password and, if it differs, the address of the coordinator they
should use on that network. The device joins the network and reconnects there.
If it cannot reach a coordinator within 60 seconds it reports failure and can
recover to the hotspot when that does not override a later manual network
choice. When Windows hides the current SSID, the client preserves that network
rather than guessing it is safe to leave. If the source coordinator cannot
confirm the destination after two minutes offline, it shows **Destination
unconfirmed**. Check the destination coordinator for actual success. Only
WPA2-PSK and open networks are supported. See the physical transfer results in
[the release report](docs/validation-0.2.3.md) and the broader hardware checklist
in [docs/manual-tests.md](docs/manual-tests.md).

## Deploying the coordinator on Linux

The release tarball contains the `coordinator` binary (web UI built in), a
systemd unit, an environment file and a Caddy example for TLS. Follow
[deploy/README.md](deploy/README.md).

## Releases and updates

Both public update keys are present in this repository. Preserve their matching
private keys outside the repository: installed clients and Managers trust those
public keys. Key generation is only needed for a new project or a planned rotation.

1. **Client update key.** Use the existing private key matching
   `release-keys/client-update.pub`. Packaging and rotation instructions are in
   [release-keys/README.md](release-keys/README.md).

2. **Manager updater key.** Use the separate Tauri key matching
   `crates/manager/updater.pub`. The signed Windows packaging command and
   verification steps are in [crates/manager/README.md](crates/manager/README.md).

3. **GitHub secrets** in the repository settings (never commit these):

   | Secret | Value |
   |---|---|
   | `SC_UPDATE_SIGNING_KEY` | the private key printed by `release-tool keygen` |
   | `TAURI_SIGNING_PRIVATE_KEY` | the content of the Tauri private key file |
   | `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | its password |

4. **Release.** Set `version` in `[workspace.package]` of `Cargo.toml`, commit,
   and push a tag `vX.Y.Z` with the same version. The release workflow builds
   everything into a draft GitHub release and publishes it only when every job
   has succeeded: the signed client, the Linux coordinator tarball, and the
   manager installers and `latest.json`.

This candidate has been packaged locally. The updated release workflow has not
been run on GitHub; verify its signing secrets before pushing a release tag
([docs/manual-tests.md](docs/manual-tests.md), group 8).

## Demo

`scripts\demo-m1.ps1` builds the workspace and runs the core loop on localhost
with `--auth token`: it enrolls this machine, runs a PowerShell job and prints
the streamed output and exit code, shows clients with a wrong secret and an
unknown key id being rejected, and shows a job queued for an offline device being
delivered when it reconnects. The key and token are generated on each run, and
state and logs go to `demo-data\m1\`.

```powershell
.\scripts\demo-m1.ps1
# if script execution is disabled for Windows PowerShell:
powershell -ExecutionPolicy Bypass -File scripts\demo-m1.ps1
```
