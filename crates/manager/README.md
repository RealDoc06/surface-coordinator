# Surface Coordinator Manager

The operator's desktop app (Tauri 2). It shows the web UI of a remote
coordinator, or runs a coordinator inside the app (with the Windows Mobile
Hotspot) for places without a network.

## Development build

From the repository root:

```
cargo build -p manager
cargo test -p manager
```

`coordinator-core` embeds `ui/dist`, so build the web UI first
(`npm ci && npm run build` in `ui/`); without it the coordinator serves a
placeholder page.

## Installers

Installers are built with the Tauri CLI, run from this directory:

```
cd ui && npm ci && npm run build && cd ..
cd crates/manager
npx --yes @tauri-apps/cli@2 build
```

This builds `manager` in release mode and bundles it for the current OS
(`bundle.targets` in `tauri.conf.json`; the CLI keeps the ones that apply):

| OS      | Installer                                   | Updater artifact (+ `.sig`)      |
|---------|---------------------------------------------|----------------------------------|
| Windows | NSIS, `bundle/nsis/*_x64-setup.exe`         | the same `-setup.exe`            |
| macOS   | `bundle/dmg/*.dmg`, `bundle/macos/*.app`    | `bundle/macos/*.app.tar.gz`      |
| Linux   | `bundle/appimage/*.AppImage`, `bundle/deb/*.deb` | the `.AppImage`             |

Paths are under `target/release/` of the workspace (or `$CARGO_TARGET_DIR`,
which must then be an absolute path). `--bundles nsis` (or `dmg`, `app`,
`appimage`, `deb`) builds only some of them. The first Windows build
downloads NSIS into the Tauri cache.

Windows specifics:

- The NSIS installer installs for the current user
  (`%LOCALAPPDATA%\Surface Coordinator Manager`) and needs no
  administrator rights.
- WebView2 is part of Windows 11 and of every serviced Windows 10. Where it
  is missing, the installer runs the WebView2 bootstrapper embedded in it
  (`embedBootstrapper`, about 2 MB) rather than first downloading the
  bootstrapper. The bootstrapper itself still needs internet to fetch the
  runtime; for machines that are offline *and* lack WebView2, build with
  `"webviewInstallMode": { "type": "offlineInstaller" }` (about 130 MB more).
- Installers are not Authenticode-signed; SmartScreen will warn on first run.

### Signing the update artifacts

`bundle.createUpdaterArtifacts` is on, so the CLI signs the updater
artifacts. It needs the private key in the environment and the public key
in `plugins.updater.pubkey`, against which it checks every signature it
makes. That field is empty in `tauri.conf.json` (builds without a key must
work), so a release build passes the committed key in:

```
export TAURI_SIGNING_PRIVATE_KEY=<key text, or path to the key file>
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=<password, empty if none>
cd crates/manager
npx --yes @tauri-apps/cli@2 build \
  --config "{\"plugins\":{\"updater\":{\"pubkey\":\"$(tr -d '\r\n' < updater.pub)\"}}}"
```

A private key that does not belong to `updater.pub` fails the build here
rather than producing updates no installed manager would accept.

For a local installer without the project key, skip the updater artifacts:

```
npx --yes @tauri-apps/cli@2 build --bundles nsis --config "{\"bundle\":{\"createUpdaterArtifacts\":false}}"
```

(or generate a throwaway key pair *outside the repository* with
`npx @tauri-apps/cli@2 signer generate -w <temp dir>/throwaway.key`, and use
it for both variables and `SC_UPDATER_PUB_FILE`, see below).

## Self-update

The manager updates itself from the repository's latest GitHub release
through `tauri-plugin-updater`: it reads
`https://github.com/RealDoc06/surface-coordinator/releases/latest/download/latest.json`
(`plugins.updater.endpoints` in `tauri.conf.json`), downloads the installer
for this OS, verifies its signature and runs it. The launcher checks when it
opens and from "Check for updates" in its footer, shows the version and
release notes, and installs only when the operator clicks "Install and
restart". Installing stops the local coordinator (and the hotspot) first.
Without internet the check fails quietly and the manager keeps working.

### The public key: `updater.pub`

Self-update is only enabled in builds that have the updater public key. It
is read at build time from `crates/manager/updater.pub` by `build.rs`:

1. The owner generates the key pair once, outside the repository:
   `npx @tauri-apps/cli@2 signer generate -w <somewhere safe>/surface-coordinator-updater.key`
2. The private key file's content and its password go into the GitHub
   Actions secrets `TAURI_SIGNING_PRIVATE_KEY` and
   `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. Never commit it.
3. The public key (`…key.pub`, one base64 line) is copied to
   `crates/manager/updater.pub` and committed.

A build without `updater.pub` works normally and says "Updates are not
configured in this build". (`plugins.updater.pubkey` in `tauri.conf.json`
stays empty on purpose: the key is compiled in from `updater.pub`.) To test
against a throwaway key without creating the file, set
`SC_UPDATER_PUB_FILE` to the path of a `.pub` file when building.
