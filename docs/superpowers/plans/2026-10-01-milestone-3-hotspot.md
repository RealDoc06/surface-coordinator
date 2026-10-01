# Milestone 3 — Hotspot: implementation plan

Date: 2026-10-01
Spec: `docs/superpowers/specs/2026-10-01-surface-coordinator-design.md`
(sections "Network state machine", "Hotspot mode", "Wi-Fi handoff")

## Goal

Hotspot control in the manager, the client network state machine, and the
`wifi.join` handoff.

## Protocol (`crates/protocol`)

- `hotspot::ssid(key_id)`: `SC-` + the first eight characters of the key id.
- `hotspot::passphrase(secret)`: HKDF-SHA-256 over the enrollment secret
  (salt `surface-coordinator/hotspot/v1`, info `wpa2-passphrase`), 15 bytes,
  encoded as 24 lowercase base32 characters (a valid WPA2 passphrase that a
  person can still type on macOS). Test vectors computed independently.
- `wifi.join` payload: `ssid`, `password` (absent for an open network),
  `coordinator_url` (optional).
- `hello.network` (optional): `ssid`, `ip`. Older coordinators ignore it.

## Client (`crates/client`)

Platform trait gains Wi-Fi methods, each defaulting to `Unsupported`:
`wifi_scan`, `wifi_join(ssid, passphrase, auto_connect)`, `wifi_current`
(SSID, address and gateway of the Wi-Fi connection).

Network state machine in the core, replacing the plain reconnect loop:

1. **Connected**: a session is up. Nothing else happens.
2. **Not connected, not yet searching**: retry the configured coordinator
   with backoff, as in milestone 1.
3. **Searching**: after 60 s without a coordinator, or at once if no
   coordinator URL is configured. Every 30 s: try the configured coordinator;
   if that fails and the device is on the hotspot SSID, try the gateway;
   otherwise scan, and if the hotspot SSID is visible, join it and try the
   gateway (`ws://<gateway>:47800`).

The client never scans or joins while a session is up. The hotspot profile is
stored as manual-connect, so Windows does not prefer it later on its own.

`wifi.join` (Wi-Fi handoff):

1. `job.started` is reported; the new coordinator URL, if given, is stored
   (`network.json` in the state directory) together with the previous one.
2. The platform joins the network (profile stored as auto-connect). A
   failure here fails the job at once, still on the old network.
3. The current session is closed and the client reconnects at once. If a
   handshake completes within 60 s the job has succeeded and is reported to
   whichever coordinator was reached.
4. Otherwise the previous coordinator URL is restored, the client goes
   straight to Searching (which rejoins the hotspot if it is there), and the
   job is reported as failed with reason `timeout`.

Timings are fields of `ClientConfig` so tests can shorten them.

Windows platform: Native Wi-Fi API (`wlanapi`) for scan, profile and connect,
`GetAdaptersAddresses` for address and gateway. No parsing of `netsh` output
(it is localised).

Tests against the fake platform and two fake coordinators ("remote" and
"gateway"): no scanning while connected; searching starts after the
threshold and immediately without a URL; hotspot joined only when visible;
gateway used when already on the hotspot; configured coordinator preferred
when it comes back; `wifi.join` success, join failure, and timeout with
return to the hotspot and URL rollback; the stored URL survives a restart.
Windows-only tests: profile XML, and read-only calls (scan, current
connection) against the real adapter. Joining a network for real is on the
manual checklist.

## Coordinator (`crates/coordinator-core`)

- `wifi.join` accepted by `POST /jobs` and validated.
- Migration 3: `devices.network_ssid`, `devices.network_ip`, filled from
  `hello.network` and shown in the device list.
- **Presumed handoff**: a `wifi.join` target still `running` whose device
  has been offline for two minutes is marked `succeeded` with a message
  saying the device left and did not return (see Deviations).

## Manager (`crates/manager`)

`HotspotControl` trait with one implementation per manager OS:

- Windows: Mobile Hotspot through the WinRT tethering API.
- Linux: NetworkManager (`nmcli device wifi hotspot`).
- macOS: manual. The manager shows SSID and passphrase and the steps to
  enable Internet Sharing.

Routes under `/api/v1/manager` (served by the embedded coordinator, same
authentication): `GET /hotspot`, `POST /hotspot/start {key_id}`,
`POST /hotspot/stop`. The embedded coordinator reports
`capabilities.hotspot`. Stopping the local coordinator or quitting the
manager stops the hotspot.

Tests: routes against a fake `HotspotControl`; SSID/passphrase taken from
the chosen key; `nmcli` argument construction; reading the Windows hotspot
state (read-only) on Windows. Starting a hotspot for real is on the manual
checklist.

## UI

Hotspot panel (when `capabilities.hotspot`): pick an enrollment key, start
and stop, SSID and passphrase, manual instructions on macOS. "Move to
Wi-Fi…" action on selected devices creating a `wifi.join` job. Network
column in the device list.

## Manual test checklist

`docs/manual-tests.md`: hotspot start/stop per manager OS, a device finding
and joining the hotspot, Wi-Fi handoff success and failure paths.

## Deviations

1. **Presumed handoff.** The spec has the client report a successful
   `wifi.join` "to whichever coordinator it reached". When that is the remote
   coordinator, the embedded coordinator that issued the job never hears and
   the job would show as running for ever. It is therefore marked succeeded
   once the device has been gone for two minutes; a device whose join failed
   is back on the hotspot within that time and reports the failure itself.
2. **Passphrase encoding.** The spec says "derived from the enrollment
   secret with HKDF" without an encoding; 24 base32 characters (120 bits).
3. **The client reports its network in `hello`** rather than in a separate
   message; a network change always breaks the connection, so every change
   is followed by a new `hello`.
4. **A handoff holds the search.** While a `wifi.join` handoff is in
   progress the connection loop does not search for the hotspot. With the
   default of 60 s for both the search threshold and the handoff timeout, the
   loop could otherwise scan, see the hotspot from the new network and rejoin
   it before the handoff had timed out.
5. **The connection generation is read after the platform join.** A session
   that came up on the old network while the join was under way would
   otherwise count as the handoff's success. Only sessions established after
   the join returned do.
6. **A Wi-Fi lock serialises the loop's scan and join with a job's join.**
   Without it the loop could scan or rejoin the hotspot in the middle of a
   `wifi.join` and undo it.
7. **Only the override URL is persisted** (`network.json`), and only once the
   join has succeeded, not when the job starts as step 1 above has it. The
   previous override is kept in memory. A client restarted during the wait
   therefore keeps the new URL, and reports the job as `interrupted` to
   whichever coordinator it reaches.
8. **The Windows client supports WPA2-PSK with AES and open networks only.**
   The profile it writes is WPA2-Personal (AES) when the job has a password
   and open otherwise. The hotspot is set to WPA2 for the same reason.
   Networks that need WPA3 only, 802.1X or WEP are not supported.
9. **The Windows hotspot tries every connection profile**, the internet one
   first. Windows ties Mobile Hotspot to a connection profile whose traffic it
   would share, and a computer in a place without internet has no internet
   profile; any profile will do, and the hotspot then has nothing to share. A
   hotspot that is already running with the same SSID and passphrase is left
   alone, since restarting it would drop the connected devices.
10. **The idle timeout is switched off while the hotspot runs.** Windows turns
    Mobile Hotspot off when no devices are connected, but devices may come
    looking for it much later. The manager disables that timeout when it
    starts the hotspot and restores it on stop, if it was on before. Stop
    does nothing when the manager did not start the hotspot (its state is
    `off`), so a Mobile Hotspot the operator uses for something else is not
    turned off.
11. **With manual control (macOS), start and stop only keep records.** There
    is no supported API for Internet Sharing. Start records the key and shows
    the SSID, passphrase and instructions; stop forgets the key. The
    operator turns the hotspot on and off.
