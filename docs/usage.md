# Using Surface Coordinator

The same interface runs in the Manager and in a browser. Keep one coordinator
as your normal destination; its devices, jobs and groups live in its database.
The Manager's local coordinator has a separate database for offline work.

## Start here

1. Open your coordinator in a browser, or launch **Surface Coordinator Manager** and choose
   **Remote coordinator**. Enter its HTTPS address and sign in.
2. Under **Administration → Enrollment keys**, create a key for your devices. Save its ID and
   secret somewhere private. A key can enroll multiple devices.
3. Copy the new `client.exe` to each Surface. In an elevated PowerShell window:

   ```powershell
   $env:SC_KEY_SECRET = '<enrollment secret>'
   .\client.exe install --key-id <key-id> --coordinator-url wss://<coordinator-host>
   Remove-Item Env:SC_KEY_SECRET
   ```

4. The device appears under **Devices**. Select it, choose **Run script…**, and
   run `Write-Output 'Connected'`. Open the job to see its output and result.

Installing starts the `SurfaceCoordinatorClient` Windows service automatically.
The service keeps running when nobody is signed in. Reinstall with the same
state directory to retain the device ID and its recorded jobs. Default paths:

| Contents | Location on the Surface |
|---|---|
| Installed executable | `%ProgramFiles%\Surface Coordinator\client.exe` |
| Configuration, identity, state and logs | `%ProgramData%\Surface Coordinator` |

Keep the default installation folders unless you have a dedicated, administrator
controlled location. Installation and removal reject shared directories and
paths that do not match the registered service. For an isolated service trial,
add `--no-hotspot-discovery` to `install`; explicit Wi-Fi moves still work.
Custom install and state directory paths must not contain `%`. Windows can
expand those characters in service commands, so installation refuses them
before changing files or the service. The default directories work as shown.

For a console-only trial, without installing a service:

```powershell
$env:SC_KEY_SECRET = '<enrollment secret>'
.\client.exe run --no-hotspot-discovery --state-dir .\client-state --key-id <key-id> --coordinator-url wss://<coordinator-host>
Remove-Item Env:SC_KEY_SECRET
```

## Everyday actions

- Search **Devices** by name, ID, Wi-Fi name or IP address. Filter by group or
  connection status. **Select online** selects the visible connected devices.
- Select the intended devices, then run a script, launch an application, apply
  a setting, move Wi-Fi, update the client, or use the power actions. Check the
  target summary before submitting. Offline targets keep their jobs queued.
- Save frequently used scripts in **Scripts** and devices in **Groups**.
- Open **Jobs** for per-device progress, streamed output, exit codes and errors.
  **Cancel job** stops a script and its child processes. Cancellation persists across
  connection loss and coordinator restart. Changes that already happened cannot
  be undone by cancellation.
- Watch the connection indicator. **Reconnecting** means the displayed device
  data may be stale. Use **Reconnect now** to retry immediately. If a write times out,
  check Jobs before submitting again because the coordinator may have accepted it.

For an interactive script, choose **Logged-in user**. **Launch app** always uses
the signed-in user's desktop. A service's **System** context has no interactive
desktop. User actions need somebody signed in to the Surface.

Power jobs report Windows errors in the job result. **Restart** and **Shutdown**
succeed when Windows accepts the scheduled action, with a five-second delay;
check the device reconnects after restarting. **Sleep** stays running while the
device is asleep and finishes after Windows resumes. Unsupported sleep fails
without suspending the device. An interrupted client reports the unfinished
action as interrupted on its next start, without repeating the power action.

## Wi-Fi without surprise disconnects

Version 0.2.3 and later preserve an active or connecting Wi-Fi connection while retrying
the coordinator. An unavailable server does not authorize a network change.
If Windows cannot report Wi-Fi state, the client preserves the connection.
An explicit **Move to Wi-Fi…** command is what authorizes a network move.

For offline work:

1. In Manager, choose **This computer** and start the local coordinator.
2. Import the same enrollment key used by your Surfaces. To synchronize keys
   instead, first sign in through **Remote coordinator**, then return to the
   launcher and open the local coordinator.
3. Under **Administration → Hotspot**, choose the key and click **Start hotspot**.
   Keep Manager running. If Windows asks for network access, allow Manager on
   the trusted private network so devices can reach port 47800.
4. A disconnected enrolled Surface can find and join this hotspot. A Surface
   already connected to another Wi-Fi stays there. To bring that device onto the
   hotspot, choose the shown hotspot in Windows or explicitly disconnect its
   current network so discovery can run.

Windows Location restrictions can hide SSIDs. The client uses adapter state to
avoid mistaking a hidden name for a disconnected device. A blank SSID in the UI
does not by itself mean Wi-Fi is broken.

To transfer devices back to your normal Wi-Fi, select them and use
**Move to Wi-Fi…**. In local Manager mode the shortcut for the Manager's current
Wi-Fi fills its credentials when Windows permits it. Use the actual SSID,
not Windows's profile label. Enter the destination coordinator's `wss://` URL
when it differs from the current one, and ensure its enrollment key matches.
If the Manager's network or destination changes after you open the confirmation,
the move is refused. Review the refreshed destination and confirm again.

Joining Wi-Fi can take tens of seconds because Windows and the access point
must authenticate and assign an address. If the destination coordinator cannot
be reached within 60 seconds, the client attempts recovery. A later manual
network choice is preserved. If Windows hides the current SSID, automatic
rollback cannot safely identify that network and leaves it connected; select
the hotspot manually if needed. Keep the hotspot running during a transfer.

If a device moves to a different coordinator, confirm it is **online there**.
The source cannot verify the destination. After its grace period it shows
**Destination unconfirmed**, rather than claiming that an absent device succeeded.

## Updates

Use **Updates** to upload the signed client executable and its signature. The
client checks the signature, hash and version before accepting it. Check the
**Automatic updates** setting before uploading: when enabled, a new upload
starts a staged rollout, first to one device and then to the rest.

For a controlled first deployment, leave automatic updates off, update one
test Surface, confirm it reconnects on the new version, then update the fleet.
Old clients built without the project update key need a manual reinstall first.
Before replacing its executable, the client also checks that the signed candidate
can start and report the expected version. After replacement, the new client
must authenticate to its coordinator within three minutes. Otherwise it restores
the previous executable and restarts the service. A prolonged network outage
during that provisional start can therefore revert an update; outages after a
successful confirmation do not trigger rollback.

If power is lost at the exact executable-replacement step and the service cannot
start, an administrator may need to recover it manually. Check the service's
registered installation folder, stop the service, and only if `client.exe` is
missing, copy the known previous `client.old.exe` back to `client.exe`. Keep the
backup and the state directory intact, then start the service and verify the
device returns. Do not use `client.failed.exe` or an unverified temporary file.
If the previous file is unavailable, reinstall a verified release while keeping
the existing state directory.

The Manager has a separate signed updater. On its launcher, choose **Check for
updates**, then **Install and restart** when a newer published release appears.
An update stops the local coordinator and hotspot, so finish local device work
first. Keep the downloaded installer available for manual reinstall or rollback.
Updater signatures verify the package and its declared version; Windows publisher
signing is separate, and these installers are not Authenticode-signed.

The standalone coordinator does not replace itself. Back up its data and upgrade
its server binary or Docker image using the [deployment guide](../deploy/README.md).
Manager updates also update its embedded local coordinator. A remote coordinator
has its own deployment. Public update discovery requires a published GitHub
release; the local bundle also supports manual installation and signed client upload.

## Troubleshooting

| Symptom | Check |
|---|---|
| Device never appears | The service is running; coordinator URL and enrollment key match; check **Rejected connections** and `client.log`. |
| Manually joined Wi-Fi drops with an old client | Install 0.2.4 or later on the Surface. Updating only the web UI cannot change old client networking. |
| Wi-Fi is connected but device is offline | Coordinator address, DNS, TLS trust and firewall. An existing Wi-Fi connection is intentionally preserved while the server is unavailable. |
| Hotspot client connects but cannot reach Manager | Allow Manager on the private network. Check VPN/subnet route warnings; a VPN route can capture replies to local devices. |
| Move shows Destination unconfirmed | Open the destination coordinator and check the device there. It can be connected successfully even though the source cannot confirm it. |
| User action reports no user session | Sign in on the Surface, or use System for an action that does not need a desktop. |
| Updates are unavailable | Verify signing keys in the build and inspect **Updates** for the reported reason. |

Stop the client temporarily from elevated PowerShell:

```powershell
Stop-Service SurfaceCoordinatorClient
# Also prevent it starting automatically, if desired:
Set-Service SurfaceCoordinatorClient -StartupType Disabled
# Re-enable it:
Set-Service SurfaceCoordinatorClient -StartupType Automatic
Start-Service SurfaceCoordinatorClient
```

`client.exe uninstall` removes the service and installed binary. Use `--purge`
only when you also want to delete its configuration and identity.

## Build and validate locally

From the repository root:

```powershell
Push-Location ui
npm ci
npm test
npm run build
Pop-Location
cargo test --locked
cargo test --locked -p manager
cargo build --locked --release -p coordinator -p client -p manager -p release-tool
.\scripts\prod-validation.ps1 -KeepRunning
```

The validation script uses a fresh folder under `demo-data/prod-validation`,
binds only to `127.0.0.1:18081`, and exercises harmless jobs with a real console
client on the current Windows PC. `report.json` records results and timings.
Automatic hotspot discovery is disabled for this test client with
`--no-hotspot-discovery`, so coordinator restarts cannot change its Wi-Fi.
With `-KeepRunning`, `access.json` records the temporary browser account and
the two process IDs to stop after inspection. These files are ignored by Git.
Without that option, both processes are stopped when validation finishes.

The local URL is reachable on the machine running the script. It is not a
deployment URL for other machines. See [deployment instructions](../deploy/README.md)
for the Linux coordinator and [manual checks](manual-tests.md) for hardware tests.
