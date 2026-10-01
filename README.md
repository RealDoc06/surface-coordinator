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

- Design: [docs/superpowers/specs/2026-10-01-surface-coordinator-design.md](docs/superpowers/specs/2026-10-01-surface-coordinator-design.md)
- HTTP API: [docs/api.md](docs/api.md)
- Plans, with the deviations from the design: [milestone 1](docs/superpowers/plans/2026-10-01-milestone-1-core-loop.md),
  [2](docs/superpowers/plans/2026-10-01-milestone-2-ui-and-manager.md),
  [3](docs/superpowers/plans/2026-10-01-milestone-3-hotspot.md),
  [4](docs/superpowers/plans/2026-10-01-milestone-4-full-job-set.md),
  [5](docs/superpowers/plans/2026-10-01-milestone-5-packaging-and-updates.md)
- Checks that need real hardware: [docs/manual-tests.md](docs/manual-tests.md)

## Status

All five milestones are implemented and covered by automated tests, and the
core was exercised end to end on one Windows 11 machine (coordinator, two
clients, the web UI driven through a browser, the manager window).

**Built but NOT verified on real hardware or a real release.** None of this
has run outside the automated tests and the checks below, and every item is on
[docs/manual-tests.md](docs/manual-tests.md):

- The Windows Mobile Hotspot, the Linux (NetworkManager) hotspot and the macOS
  manual hotspot instructions.
- A device finding the hotspot, joining it, and the Wi-Fi handoff (`wifi.join`).
- The client as a Windows service: `client.exe install` and `uninstall`,
  restart after a crash, running at the sign-in screen.
- Jobs that need the service identity: scripts and apps run as the logged-in
  user, user-scope settings, and everything with nobody logged in.
- Power jobs (restart, shut down, sleep) and every setting other than
  `audio.volume` and `audio.mute`.
- Client updates (download, signature check, swap, rollback), the release
  workflow, and the manager installers and self-update. Until the signing keys
  exist (see "Releases and updates") builds have updates disabled.
- The coordinator behind Caddy on a Linux server, and the macOS and Linux
  managers.

| Milestone | Scope | State |
|---|---|---|
| 1 Core loop | Protocol, enrollment-key handshake, heartbeats, `script.run` with streamed output | Done; verified live on localhost |
| 2 UI and manager | Accounts and login, web UI, device list, jobs, the manager shell | Done; UI and manager verified live on Windows |
| 3 Hotspot | Hotspot control, client network state machine, `wifi.join` | Built; hotspot and Wi-Fi steps not verified on hardware |
| 4 Full job set | Script library, groups, app launch, settings catalogue, power, cancel | Done; script, launch, volume and cancel verified live; the rest not verified on hardware |
| 5 Packaging and updates | Client as a service, signed updates, release workflow, installers, Linux deployment | Built; not verified on real machines or a real release |

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
release-keys/            the client update public key, once generated
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
   .\target\debug\client.exe --coordinator-url ws://127.0.0.1:8080 --key-id <key id> --state-dir demo-data\client
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
Installing and running as a service is **not verified on real hardware**; it is
on the manual checklist, group 4.

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
**Enrollment keys**, open **Hotspot**, pick the key and **Start**. The SSID is
`SC-` plus the start of the key id and the passphrase is derived from the key
secret; both are shown. On Windows and on Linux with NetworkManager the manager
turns the hotspot on itself; on macOS it shows the steps for Internet Sharing.
Devices enrolled with that key and with no reachable coordinator scan every 30
seconds, join the hotspot and appear in the device list.

Wi-Fi handoff: select the devices and choose **Move to Wi-Fi...** with the
SSID, the password and, if it differs, the address of the coordinator they
should use on that network. The device joins the network and reconnects there.
If it cannot reach a coordinator within 60 seconds it goes back to searching,
rejoins the hotspot and reports the job as failed. When the new coordinator is
a different one, the job that started on the hotspot is marked succeeded once
the device has been gone for two minutes. Only WPA2-PSK and open networks are
supported. This flow is **not verified on hardware**.

## Deploying the coordinator on Linux

The release tarball contains the `coordinator` binary (web UI built in), a
systemd unit, an environment file and a Caddy example for TLS. Follow
[deploy/README.md](deploy/README.md).

## Releases and updates

Until the owner has done the one-time steps below, builds carry no update key:
the coordinator does not look for releases, clients refuse every update, and
the manager says "Updates are not configured in this build". Everything else
works.

1. **Client update key.** Generate it once with `release-tool` and commit the
   public half; the tool prints the private half once and never writes it to a
   file. Details and key rotation are in [release-keys/README.md](release-keys/README.md).

   ```
   cargo run -p release-tool -- keygen --public-out release-keys/client-update.pub
   ```

2. **Manager updater key.** Generate a Tauri updater key pair outside the
   repository and commit the public key as `crates/manager/updater.pub`; see
   [crates/manager/README.md](crates/manager/README.md):

   ```
   npx @tauri-apps/cli@2 signer generate -w <somewhere safe>/surface-coordinator-updater.key
   ```

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

The release workflow has never run for real. Do the dry run on a fork first
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
