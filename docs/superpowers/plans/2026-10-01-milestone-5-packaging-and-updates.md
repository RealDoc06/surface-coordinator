# Milestone 5 — Packaging and updates: implementation plan

Date: 2026-10-01
Spec: `docs/superpowers/specs/2026-10-01-surface-coordinator-design.md`
(sections "Client", "Releases and self-update", "Error handling")
API contract: `docs/api.md`, section "Milestone 5 additions"

## Goal

Client installer and service registration, coordinator deployment for Linux,
manager installers, the signed release workflow, manager self-update and
`client.update`.

## Signing (`crates/protocol::update`, `crates/release-tool`)

- Client update artifacts are signed with Ed25519. The signature covers
  `"surface-coordinator/client-update/v1" 0x00 version 0x00 target 0x00
  sha256(file)`, so a signature cannot be moved to another version or target.
  Signature and public key travel as base64.
- `protocol::update`: `verify(public_key, version, target, sha256, signature)`,
  `signing_message(..)`, version comparison (`major.minor.patch`).
- `crates/release-tool` (used by CI and by the owner): `keygen`, `sign`,
  `verify`. The private key comes from the environment
  (`SC_UPDATE_SIGNING_KEY`), never from a file in the repository.
- The public key is the file `release-keys/client-update.pub`, compiled into
  the client and the coordinator by their build scripts. **Until the owner
  generates a key pair and commits the public key, builds have no update key
  and refuse every client update.** Tests generate throwaway keys at run time.

## Client (`crates/client`)

- **Configuration file** (`config.json` in the state directory, written by
  `install`): device id, enrollment key id and secret, coordinator URL
  (optional). Command-line flags and environment variables still work for
  console use.
- **Windows service**: `client.exe service` runs under the service control
  manager (LocalSystem, automatic start, restart on failure).
  `client.exe install` copies the executable to
  `%ProgramFiles%\Surface Coordinator\`, writes the configuration to
  `%ProgramData%\Surface Coordinator\` with access limited to SYSTEM and
  Administrators, registers and starts the service. `client.exe uninstall`
  reverses it. Both need an elevated prompt.
- **Log file**: a size-capped rotating file in the state directory when
  running as a service.
- **`client.update`** exactly as in the spec: download from the coordinator
  (`/updates/client/<version>/<target>` and `.sig`) to a temporary file next
  to the executable; check size, SHA-256 and signature; refuse a version
  older than the running one unless `downgrade` is set; swap executables and
  write the pending-update marker; report `running` with a "restarting" note
  and exit; on start, complete the update after the first successful
  handshake (delete marker and old executable, report succeeded with the new
  version); after three starts without a handshake restore the old executable
  and report failed.
  The decision logic lives in the core against the platform trait (file
  operations and restart), so it is tested with a fake platform.

## Coordinator (`crates/coordinator-core`, `crates/coordinator`)

- **Release cache**: periodically and on demand, read the latest release of
  the configured GitHub repository, download the client artifact and its
  signature into `<data dir>/updates/<version>/`, verify the signature, and
  keep it. Works the same in the embedded coordinator while the manager has
  internet, so a cached version can be pushed later on the hotspot.
- `/updates/client/<version>/<target>` (and `.sig`) on the client listener.
- `client.update` job kind; devices report their version, the API marks the
  ones older than the newest cached version.
- **Version compatibility**: the coordinator accepts the current and the
  previous protocol version. A client on any other version connects in a
  restricted state in which the only job dispatched to it is `client.update`.
- **Deployment for Linux**: `deploy/coordinator.service` (systemd, dedicated
  user, hardening), `deploy/Caddyfile.example` for TLS, `deploy/README.md`.

## Manager (`crates/manager`)

- Installers through the Tauri bundler: NSIS on Windows, DMG on macOS,
  AppImage and deb on Linux.
- Self-update with the Tauri updater against the repository's latest
  release: checked at startup and on demand from the launcher, shows version
  and notes, installs when the operator confirms. No check without internet.

## Release workflow (`.github/workflows/`)

- `ci.yml`: build and test on every push and pull request.
- `release.yml` on a `vX.Y.Z` tag: check that the tag matches the workspace
  version; build the client (Windows), the coordinator (Linux) and the
  manager (Windows, macOS, Linux); sign the client artifact with
  `release-tool` and the manager bundles with the Tauri signer; publish a
  GitHub Release with all artifacts, signatures and the updater manifest.
  Secrets: `SC_UPDATE_SIGNING_KEY`, `TAURI_SIGNING_PRIVATE_KEY`,
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

## UI

Updates page: newest cached client version, when it was checked, "Check
now"; per-device version with an "update available" marker and a
"restricted" marker; "Update clients…" for selected devices, a group or all.

## What cannot be verified in this repository's development environment

Service installation (needs elevation), the executable swap under the
service manager, the release workflow (needs a pushed tag and the secrets),
macOS and Linux builds and installers. These go on the manual checklist in
`docs/manual-tests.md`.

## Deviations

1. **The client installs itself** (`client.exe install`) instead of shipping
   an MSI. One file can be run from a USB stick or an imaging script, and no
   installer toolchain is needed in CI.
2. **The signature covers version and target as well as the file hash**; the
   spec only says artifacts are signed.
3. **The coordinator verifies signatures too**, before caching an artifact,
   although only the client's check is required by the spec.
