# Manual test checklist

Everything here needs real hardware, a real operating system service
manager, a real network, or a pushed release. None of it runs in the automated
suite. Work through the groups before a release; tick each box and note the
version, the machines and anything unexpected.

```
Version tested:        Date:        Tester:
Manager machines:      Windows / Linux / macOS (versions)
Devices:               (model, Windows build)
```

Conventions:

- "Manager" is the desktop app; "embedded coordinator" is the local
  coordinator it starts ("This computer" on the launcher page). "Remote
  coordinator" is the server from `deploy/README.md`.
- "The UI" is the web UI, reached through the manager or a browser.
- Job states are `queued`, `sent`, `running`, `succeeded`, `failed`,
  `expired`. Failure reasons are shown with the failed target.
- Test devices should be machines you can lose network access to or reboot
  without consequences. Never run the power tests on a device with unsaved
  work.

## Prerequisites

- [ ] A device (Windows, Wi-Fi adapter) with the client installed as a service
  (group 4) and enrolled with an enrollment key created in the UI.
- [ ] A second Wi-Fi network ("the other network") with internet access, whose
  name and password you know, and that the device can reach.
- [ ] For group 9: a Linux server with a DNS name, ports 80 and 443 open.

## 1. Hotspot on the manager

The hotspot and the embedded coordinator start and stop together. The SSID is
`SC-` followed by the first eight characters of the enrollment key id, the
passphrase is shown in the UI. Devices connect to the embedded coordinator on
the client port shown (47800 by default).

### 1.1 Windows manager

- [ ] Start "This computer" in the launcher, open the UI, go to **Hotspot**,
  pick an enrollment key, **Start**. Expected: state `on`, SSID `SC-xxxxxxxx`
  and a 24-character passphrase shown; Windows Settings > Network > Mobile
  hotspot shows the hotspot on with that name and password; a phone can see
  and join the network with that passphrase.
- [ ] **Stop**. Expected: state `off`; Mobile hotspot is off in Windows
  Settings; the SSID disappears from a phone's scan.
- [ ] Start the hotspot again, then click **Stop** in the launcher for the local
  coordinator (or quit the manager). Expected: the hotspot turns off too.
- [ ] Start with a revoked key. Expected: refused with a clear message, hotspot
  stays off.
- [ ] Start on a machine with Wi-Fi turned off, or with no connection to
  share. Expected: an error message that says why (`hotspot_failed`), state
  `error`, nothing left running. Record the exact message.

### 1.2 Linux manager (NetworkManager)

- [ ] Same flow as 1.1 on a Linux desktop with a Wi-Fi adapter that supports
  access-point mode. Expected: state `on`; `nmcli connection show
  --active` lists `surface-coordinator-hotspot`; a phone can join with the
  shown passphrase.
- [ ] **Stop**. Expected: the `surface-coordinator-hotspot` connection is
  inactive and the SSID is gone.
- [ ] Quit the manager while the hotspot is on. Expected: hotspot stops.
- [ ] Run on a machine without NetworkManager (or with `nmcli` missing from the
  `PATH`). Expected: start fails with "could not run NetworkManager's
  nmcli"; nothing is left running.
- [ ] Note what happens to the machine's own Wi-Fi connection when it has only
  one adapter (NetworkManager may drop it). Record it.

### 1.3 macOS manager (manual Internet Sharing)

- [ ] **Hotspot** page shows that Internet Sharing has to be enabled by hand,
  with the SSID, the passphrase and the steps. Expected: **Start** returns
  state `on` immediately without changing the system.
- [ ] Follow the steps exactly as written (System Settings > Sharing >
  Internet Sharing, share to Wi-Fi, network name = the SSID, WPA2 security,
  the passphrase). Expected: the steps are correct and complete for the
  macOS version in use; a phone can join the network. Note any wording that is
  out of date.
- [ ] A device joins this hotspot and appears in the UI (group 2). Expected:
  works the same as with the Windows hotspot.
- [ ] **Stop**. Expected: the manager state returns to `off`; Internet Sharing
  stays on until turned off by hand (the page says so).

## 2. A device finds and joins the hotspot

Use a device whose enrollment key is the one the hotspot was started for, and
that has no reachable coordinator (so it is Searching: either no coordinator
URL configured, or the remote is unreachable).

- [ ] Hotspot on, device in range. Expected: within about a minute (it scans
  every 30 s) the device joins `SC-xxxxxxxx` and appears **online** in the UI
  of the embedded coordinator; the device list's network column shows the
  hotspot SSID and its IP address.
- [ ] Run a PowerShell job (`Write-Output hello`) on it. Expected: `succeeded`,
  output arrives live.
- [ ] The device was configured with a coordinator URL and that remote
  coordinator is working, hotspot on and in range. Expected: the device stays
  on its network and on the remote coordinator; it never joins the hotspot
  while connected.
- [ ] Take that device off its network (turn off the router or the device's Wi-Fi
  and back on to a profile that cannot connect). Expected: after about 60 s
  without a coordinator it starts scanning and joins the hotspot.
- [ ] Hotspot off, device Searching. Expected: it keeps scanning every 30 s and
  does not join anything else.
- [ ] After the device left the hotspot (group 3), check Windows' saved Wi-Fi
  networks. Expected: the hotspot profile is set to connect manually, so
  Windows does not prefer it when it is in range and the device is online
  elsewhere.

## 3. Wi-Fi handoff (`wifi.join`)

Start from a device online through the embedded coordinator on the hotspot.
In the UI select it and choose **Move to Wi-Fi...**. Keep the hotspot on
during the failure tests.

### 3.1 Success

- [ ] Enter the other network's SSID and password and a coordinator URL
  (`wss://<remote host>`) that is reachable from that network. Expected: the
  job turns `running`, the device leaves the hotspot, joins the other network
  and appears online in the **remote** coordinator's UI (new network SSID and
  IP shown).
- [ ] Back in the embedded coordinator's UI, the job stays `running` until the
  device has been gone for two minutes, then becomes `succeeded` with a
  message that the device left and did not return.
- [ ] Reboot the device. Expected: it reconnects to the remote coordinator
  (the new URL was stored and the profile auto-connects).
- [ ] Repeat without a coordinator URL, with the other network having a
  route to the same coordinator. Expected: the device connects to its
  previously configured coordinator over the new network.
- [ ] Repeat with an open network (no password). Expected: joins.

### 3.2 Wrong password

- [ ] Send `wifi.join` with the right SSID and a wrong password. Expected: the
  job ends `failed` with a message from the join; the device is back on (or
  never left) the hotspot and shows **online** in the embedded coordinator
  again within about a minute. Record how long the failure took and the
  message.
- [ ] The device's stored coordinator URL after the failure. Expected: unchanged
  from before the job (a reboot still connects to the old one).

### 3.3 Coordinator unreachable on the new network

- [ ] Send `wifi.join` with the correct SSID and password but a coordinator URL
  that cannot be reached (an unused host name, or the remote service
  stopped). Expected: the device joins the other network, finds no
  coordinator for 60 s, goes back to Searching, rejoins the hotspot (it must
  be on and in range), and the job ends `failed` with reason `timeout`,
  reported to the embedded coordinator.
- [ ] The device's stored coordinator URL after this. Expected: restored to the
  previous one.
- [ ] Same test with the hotspot turned off. Expected: the device keeps
  searching, no job result can arrive; when you start the hotspot later it
  rejoins and reports the failure.

## 4. Client service (Windows)

Use a throwaway Windows machine or VM. The installer needs an elevated prompt.
Use the install and uninstall flags documented by `client.exe --help`; give it
the enrollment key id and secret (and optionally the coordinator URL).

- [ ] `client.exe install` from a **non-elevated** prompt. Expected: refused
  with a clear message; nothing was installed.
- [ ] `client.exe install` from an elevated prompt. Expected:
  - `%ProgramFiles%\Surface Coordinator\client.exe` exists;
  - `%ProgramData%\Surface Coordinator\` holds the configuration, and its
    permissions (`icacls`) allow only SYSTEM and Administrators;
  - the service is registered with start type Automatic, runs as LocalSystem
    (`sc.exe qc <name>`), and is running; recovery actions restart it on failure;
  - the device appears online in the UI.
- [ ] Kill the client process in Task Manager. Expected: the service manager
  restarts it and the device comes back online.
- [ ] Reboot with nobody logged in. Expected: the device comes online at the
  sign-in screen.
- [ ] A log file exists in the state directory and stays below its size cap after
  a few thousand lines (use `--verbose`-level traffic or repeated jobs);
  rotated files are replaced rather than growing without limit.
- [ ] Run `client.exe install` a second time. Expected: a clean error or an
  in-place reinstall, never a half-installed state.
- [ ] `client.exe uninstall` from an elevated prompt. Expected: the service is
  stopped and removed (`sc.exe query` no longer finds it), the installed
  executable is gone as the command's output describes, the device goes
  offline in the UI. The configuration is removed or kept as the command says.
- [ ] `client.exe uninstall` when not installed. Expected: a clean message.

### 4.1 Identity and user session

Run these jobs from the UI against the installed (LocalSystem) client.

- [ ] Script `whoami`, run as **system**. Expected: `nt authority\system`.
- [ ] Same script, run as **user**, with a user logged in on the console.
  Expected: `<DOMAIN-or-PC>\<username>` of that user; the environment
  (`$env:USERPROFILE`) is the user's profile.
- [ ] **Launch app**: `C:\Windows\System32\notepad.exe` with argument
  `C:\Windows\win.ini`. Expected: Notepad opens on the logged-in user's
  desktop (visible, in the user's session, not session 0) showing the file;
  the job is `succeeded`.
- [ ] Launch an app that does not exist. Expected: `failed` with a clear message.
- [ ] Sign out so nobody is logged in (sign-in screen), then run a script as
  **user** and an **app launch**. Expected: both `failed` with reason
  `no_user_session`. A script run as **system** still succeeds.
- [ ] Lock the screen with a user logged in, run the user script and the app
  launch. Record the behaviour (the active console session is still that
  user's, so it should succeed).
- [ ] Two users with fast user switching, the second one active. Expected: jobs
  run as the user of the **active console** session.
- [ ] Run a long script (`Start-Sleep 600`) and **cancel** it in the UI.
  Expected: `failed` with reason `cancelled`, the PowerShell process and its
  children are gone from Task Manager, the next job on the device runs.

## 5. Settings catalogue on real hardware

Use the **apply setting** composer on a Surface (brightness only works on a
device with a controllable panel). For each entry apply a value, check the
effect, then apply the original value back. Expected for all: the job is
`succeeded`; the visible effect appears; out-of-range values are refused by the
UI/API before dispatch.

- [ ] `audio.volume` (user scope, 0-100): master volume changes to the percentage
  shown in the system volume flyout.
- [ ] `audio.mute` (user): toggles mute for the logged-in user.
- [ ] `display.brightness` (machine, 0-100): panel brightness changes.
- [ ] `power.display_timeout_ac` and `power.display_timeout_battery` (minutes, 0 =
  never): Settings > System > Power shows the new values for the right power
  source; `powercfg /query` agrees.
- [ ] `power.sleep_timeout_ac` and `power.sleep_timeout_battery`: same check.
- [ ] `desktop.wallpaper` (user, path on the device): the wallpaper changes for the
  logged-in user; a path that does not exist fails with a clear message.
- [ ] `windows_update.pause_days` (0-35): Windows Update shows updates paused
  until the expected date; `0` resumes.
- [ ] `taskbar.locked` (user): the taskbar lock state changes (note whether
  Explorer has to restart or the user has to sign out for it to show).
- [ ] `start.layout_locked` (user): the Start layout can no longer be changed (note
  any sign-out requirement).
- [ ] `time.zone` (Windows time zone id, for example `W. Europe Standard Time`):
  the clock shows the new zone; an invalid id fails with a clear message.
- [ ] Apply a user-scope setting with nobody logged in. Expected: `failed` with
  reason `no_user_session`.
- [ ] Apply a setting whose value contains quotes or `$(...)`/`;` (for
  example as the wallpaper path). Expected: treated as plain text, no code
  runs.

## 6. Power actions

Each power job asks for confirmation in the UI. The client reports the result
first and carries the action out once the coordinator has acknowledged it, or
after 5 seconds.

- [ ] **Restart**: the job shows `succeeded` (it is not left `running`), the device
  goes offline about five seconds later, restarts and comes back online by
  itself (service starts at boot).
- [ ] **Shutdown**: job `succeeded`, device goes offline and stays off. Power it
  on and check that it reconnects.
- [ ] **Sleep**: job `succeeded`, the device sleeps and shows offline after the
  heartbeat timeout; waking it brings it back online.
- [ ] Power job for an offline device. Expected: stays `queued` and runs when the
  device returns (and expires after the configured hours otherwise).
- [ ] Cancel a power job that is still `queued`. Expected: `failed` / `cancelled`,
  the device does nothing when it comes back.

## 7. Client update

Needs a release published to the repository the coordinator watches (use the
fork of group 8 for test releases) containing the client artifacts
`surface-coordinator-client-<version>-windows-x86_64.exe` and `.exe.sig`, built
with a client that carries the matching public key.

- [ ] **Check now** on the Updates page. Expected: the newest version, its size,
  and the time of the check appear; the artifact and signature are cached
  under `<data dir>/updates/<version>/` (an artifact whose signature does not
  verify is never cached).
- [ ] The device list marks devices older than the cached version "update
  available". Devices on the newest version are not marked.
- [ ] **Happy path.** Run **Update clients...** for one device (an installed
  service on an older version). Expected, in order: the job goes `running`
  with a "restarting" note; the device goes offline briefly; it returns
  reporting the new version; the job ends `succeeded` with the new version;
  `client.old.exe` and the pending-update marker are gone from the
  installation directory; the service is still LocalSystem and automatic.
- [ ] Update a group and then **all** devices. Expected: every online device ends
  `succeeded`; offline devices stay `queued` and update when they return.
- [ ] Update a device that is already on that version. Expected: `succeeded` with
  a message saying so, no restart.
- [ ] **Downgrade refused.** Job with an older version, downgrade not set.
  Expected: `failed`, reason `downgrade_refused`, the client keeps running.
  With the downgrade flag set: the device installs it.
- [ ] **Bad signature rejected.** Replace the cached `.sig` file in
  `<data dir>/updates/<version>/` with a signature made by a different key
  (`release-tool sign` with another `SC_UPDATE_SIGNING_KEY`), then update a
  device. Expected: `failed`, reason `bad_signature`; the device did not
  restart and its executable did not change.
- [ ] **Bad hash.** Change one byte of the cached executable (same length) and
  update a device. Expected: `failed`, reason `bad_hash`; nothing installed.
  Restore the file afterwards.
- [ ] **Download failure.** Block the device from the coordinator's update URL
  (or delete the cached file) mid-job. Expected: `failed`, reason
  `download_failed`; the device keeps working on the old version.
- [ ] **Rollback after three failed starts.** Release a deliberately broken
  version (a throwaway build that exits at start-up, signed with the test
  key). Update a device to it. Expected: the service restarts the new
  executable three times without a handshake, then restores
  `client.old.exe`, restarts, the old version comes online and the job ends
  `failed` with reason `rolled_back`.
- [ ] Interrupt the update by powering the device off right after the swap.
  Expected: on the next boot the new version starts and either completes
  (handshake) or rolls back after three tries; never a device that is
  permanently offline.
- [ ] A client with a protocol version the coordinator no longer supports (an
  old release). Expected: it connects in a restricted state, is marked
  "restricted" in the device list, runs no job except `client.update`, and
  becomes normal after the update.
- [ ] Run the update through the embedded coordinator: the manager has internet,
  caches the release, then switch to the hotspot and push the update to a
  device that is on the hotspot. Expected: works without internet.
- [ ] With the internet blocked, **Check now** reports a clear error
  (`update_check_failed`); the last cached version stays available.

## 8. Release workflow (dry run on a fork)

Never test with the real keys. Use a fork and throw-away keys; the workflow is
not exercised by the automated tests.

Preparation:

- [ ] Fork `RealDoc06/surface-coordinator`. Enable Actions on the fork.
- [ ] `cargo run -p release-tool -- keygen --public-out release-keys/client-update.pub`
  and add the printed private key as the fork's secret `SC_UPDATE_SIGNING_KEY`.
- [ ] Generate the manager's updater keys (`npx @tauri-apps/cli@2 signer
  generate`), put the public key in `crates/manager/updater.pub`, and add the
  private key and its password as the secrets `TAURI_SIGNING_PRIVATE_KEY` and
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
- [ ] Point the manager's updater endpoint and the coordinator's release source
  at the fork (otherwise they look at the upstream repository).
- [ ] Commit the two public keys on the fork's `main`.

Negative runs (each should stop early and leave no published release):

- [ ] Push a tag that does not match the workspace version (`v9.9.9` while
  `Cargo.toml` says `0.1.0`). Expected: `prepare` fails with a message naming
  both versions; no builds start.
- [ ] Remove `release-keys/client-update.pub` on a branch and tag it. Expected:
  `prepare` fails with "generate the release keys first, see
  release-keys/README.md".
- [ ] Set `SC_UPDATE_SIGNING_KEY` to a different key than the committed public
  key. Expected: the `client` job fails at the key match check, before
  signing; nothing is uploaded.
- [ ] Re-push a tag for a version that is already published. Expected: `prepare`
  refuses.

Positive run (bump the version in the root `Cargo.toml`, commit, tag
`vX.Y.Z`, push the tag):

- [ ] `prepare` passes, builds the UI, and creates a **draft** release with
  generated notes.
- [ ] `client` builds, signs and verifies. The draft release gets
  `surface-coordinator-client-X.Y.Z-windows-x86_64.exe` and
  `surface-coordinator-client-X.Y.Z-windows-x86_64.exe.sig`.
  Verify locally with `release-tool verify --file ... --version X.Y.Z --target
  windows-x86_64 --signature ... --public-key release-keys/client-update.pub`.
- [ ] `coordinator` attaches
  `surface-coordinator-coordinator-X.Y.Z-linux-x86_64.tar.gz`. Unpack: it
  contains `coordinator`, `coordinator.service`, `coordinator.env.example`,
  `Caddyfile.example` and `README.md`; `./coordinator --version` prints
  `X.Y.Z`; the web UI served at `/` is the real UI, not the "web UI was not
  built into this binary" placeholder.
- [ ] `manager` jobs run for Windows, macOS and Linux and attach the NSIS `.exe`,
  `.dmg`, `.AppImage`, `.deb`, the updater archives and `.sig` files, and
  `latest.json`.
- [ ] `latest.json` has `version` `X.Y.Z`, platform entries for
  `windows-x86_64`, `darwin-aarch64` and `linux-x86_64`, and download URLs
  that point at the tag (no `untagged-` in them) and work after publishing.
- [ ] `publish` runs only after all jobs succeeded and turns the draft into the
  latest release. Before that, the release was not visible to anonymous users.
- [ ] Break one job on purpose (for example a failing macOS build). Expected:
  `publish` does not run and the release stays a draft.
- [ ] The Windows client executable and the coordinator carry the version of
  the tag (`client.exe --version`).

## 9. Manager installers and self-update

Install the test release (or the real one) on each OS from the GitHub release.

- [ ] **Windows (NSIS .exe).** Run the installer. Expected: SmartScreen warns
  (the installer is not code-signed yet) and can be bypassed; the app
  installs, starts from the Start menu, shows the launcher with the right
  version; uninstall via Settings > Apps removes it.
- [ ] **macOS (.dmg).** Open the image and drag the app to Applications.
  Expected: Gatekeeper blocks the unsigned/unnotarized app until it is
  opened with right-click > Open (or the quarantine attribute is removed);
  after that it starts and shows the launcher.
- [ ] **Linux .AppImage.** `chmod +x`, run. Expected: starts and shows the
  launcher (needs FUSE 2 on some distributions; note it if so).
- [ ] **Linux .deb.** `sudo apt install ./<file>.deb`. Expected: installs with its
  dependencies, appears in the application menu, removable with
  `apt remove`.
- [ ] On each OS: connect to a remote coordinator (https URL), log in; start "This
  computer", log in to the embedded coordinator, stop it again.
- [ ] On each OS: the **Hotspot** page appears with the right mode (Windows:
  automatic, Linux: automatic with NetworkManager, macOS: manual with
  instructions) and works as in group 1.

### Manager self-update

Needs a previous manager version installed and a newer release published
(on the fork, with the endpoint pointed at the fork).

- [ ] At start-up with internet, the launcher shows the new version and its release
  notes. **Install and restart** downloads, installs and relaunches the new
  version (launcher footer shows it).
- [ ] **Later** dismisses the notice; the footer's **Check for updates** shows it
  again on demand. With no newer version it says the app is up to date.
- [ ] With the embedded coordinator running, the notice warns that installing
  stops it; devices on the hotspot go offline until it is started again.
- [ ] No internet (hotspot mode): the manager starts normally, no update check is
  attempted at start-up, and **Check for updates** reports a clear error
  without freezing the app.
- [ ] Tamper with `latest.json` (change the signature or the download URL's file)
  on the fork's release. Expected: the update is refused, the installed
  version stays as it is.
- [ ] Do the update on all three OSes (Windows, macOS, Linux AppImage; the .deb
  install is updated through the package, note what the updater does there).

## 10. Remote coordinator behind Caddy (wss)

Follow `deploy/README.md` on a fresh Linux server.

- [ ] Install from the release tarball: the service starts, `systemctl status
  coordinator` is active, the unit runs as `surface-coordinator`, the data
  directory `/var/lib/surface-coordinator` is owned by that user with mode 0750.
- [ ] `systemd-analyze security coordinator` shows a reasonable exposure level and
  the service still works with the hardening in place (writes to the database,
  fetches a release, accepts connections).
- [ ] `coordinator account create` and `coordinator key create` (run as the service
  user, as in the README) work, with the service running.
- [ ] Caddy obtains a certificate; `https://<host>` shows the login page;
  `http://<host>` redirects to https; port 8080 is not reachable from
  outside.
- [ ] Log in. Expected: the session cookie has the Secure flag
  (`SC_SECURE_COOKIES=true`); the UI works and live updates arrive (the events
  WebSocket works through the proxy).
- [ ] Enroll a device with coordinator URL `wss://<host>` and the key. Expected:
  it appears online; a job runs and its output streams live.
- [ ] Leave a device connected for an hour. Expected: it stays online (the proxy does
  not drop the idle WebSocket).
- [ ] `systemctl restart coordinator`. Expected: devices show offline and then come
  back by themselves; the UI reconnects.
- [ ] `systemctl reload caddy`. Expected: no lasting effect on connected devices
  (they may reconnect once).
- [ ] Connect the manager to `https://<host>`: login, device list and a job work
  inside the manager window.
- [ ] Reboot the server. Expected: both services start by themselves.
- [ ] Wrong enrollment key from a device. Expected: refused, appears under
  **Rejected connections**.
- [ ] Upgrade: install a newer tarball as in the README. Expected: the service
  starts, the database migrated, accounts and devices are intact.
- [ ] Backup and restore as in the README (stop, copy the data directory, restore on
  a second machine). Expected: accounts, keys, devices and job history are
  there.
- [ ] With the server's outbound HTTPS blocked, the coordinator keeps serving; only
  the update check reports an error.
