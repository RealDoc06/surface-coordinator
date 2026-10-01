# Milestone 4 — Full job set: implementation plan

Date: 2026-10-01
Spec: `docs/superpowers/specs/2026-10-01-surface-coordinator-design.md`
(sections "Jobs", "Settings catalogue", "Client", "Data model")
API contract: `docs/api.md`, sections "Jobs", "Groups", "Scripts" and
"Settings catalogue"

## Goal

Settings catalogue, `app.launch`, `power`, script library, groups. Also what
those need to be usable: running as the logged-in user, and cancelling a job.

## Protocol (`crates/protocol`)

- Payload types: `AppLaunchPayload { path, arguments: Vec<String> }`,
  `PowerPayload { action: restart | shutdown | sleep }`,
  `SettingApplyPayload { setting: String, value: serde_json::Value }`.
- `settings` module: the catalogue as data. Each entry has an id, a label, a
  description, a value type (`bool`, `integer { min, max, unit }`, `text {
  max_len }`), and whether it applies to the machine or to the logged-in user.
  `settings::catalogue()` and `settings::validate(id, &value)`. The coordinator
  validates with it, the UI renders forms from it, the client implements it.

  | id | value | scope |
  |---|---|---|
  | `audio.volume` | integer 0–100 (%) | user |
  | `audio.mute` | bool | user |
  | `display.brightness` | integer 0–100 (%) | machine |
  | `power.display_timeout_ac`, `power.display_timeout_battery` | integer 0–600 (minutes, 0 = never) | machine |
  | `power.sleep_timeout_ac`, `power.sleep_timeout_battery` | integer 0–600 (minutes, 0 = never) | machine |
  | `desktop.wallpaper` | text: path of an image on the device | user |
  | `windows_update.pause_days` | integer 0–35 (0 = resume) | machine |
  | `taskbar.locked` | bool | user |
  | `start.layout_locked` | bool | user |
  | `time.zone` | text: Windows time zone id | machine |

- `job.cancel { job_target_id }` (coordinator → client) and failure reasons
  `cancelled` and `no_user_session`.

## Client (`crates/client`)

Platform trait gains (default `Unsupported`): `launch_app(path, arguments)`,
`apply_setting(id, value)`, `power(action)`. `run_script` honours
`run_as: user`.

- **User session.** When the client runs as LocalSystem, "as the logged-in
  user" means the active console session's token (`WTSQueryUserToken`,
  `CreateProcessAsUser`). When it runs as an ordinary user (console mode) it
  already is that user and starts the process directly. Nobody logged in →
  `failed` / `no_user_session`.
- **Settings on Windows** are small PowerShell or command snippets run through
  the existing script runner in the entry's scope. Values reach the snippet
  through environment variables, never by text substitution, so a value can
  never become code.
- **`power`**: the result is reported first and the action is carried out
  once the coordinator has acknowledged it or after 5 s, whichever is first.
- **`job.cancel`**: a waiting job is dropped, a running script is stopped
  with its process tree; either way the result is `failed` / `cancelled`.

Tests with the fake platform for every kind and for cancel; Windows-only
tests for the user-session launch in console mode and for settings that are
safe to apply and restore (volume, mute). Power actions are never executed
by a test.

## Coordinator (`crates/coordinator-core`)

- Migration: `groups`, `group_members`, `scripts`.
- Jobs: validation of the three new kinds; target type `group`; cancel.
- Groups, scripts and the settings catalogue over the API. Built-in scripts
  are seeded and read-only.
- Devices list their groups.

## UI

Groups (create, rename, delete, membership, filter the device list, target a
group); script library (browse, create, edit, delete, "use in composer",
"save to library" from the composer); a composer per job kind (script, launch
app, apply setting, power with confirmation, move to Wi-Fi); cancel on the
job page.

## Deviations

1. **Cancel** is not in the spec. A script with a long timeout that hangs
   would otherwise block every later job on that device, since jobs run one
   at a time. Cancelled targets end as `failed` with reason `cancelled`, so
   the set of states in the spec is unchanged.
2. **Wallpaper takes a path on the device.** File transfer is out of scope in
   the spec; the image has to be on the device or a share already.
3. **Jobs copy the script body.** A job created from a library script does
   not reference it, so editing or deleting a library script never changes
   what an existing job did.
4. **`taskbar.locked` and `start.layout_locked` are written by the client
   itself** to the logged-in user's hive under `HKEY_USERS\<SID>`, not run as
   the user. These are per-user policy keys, which the user can read but not
   write. The client runs the snippet as LocalSystem with the SID in
   `SC_USER_SID`. Explorer reads the policy at sign-in, so the change shows
   from the next sign-in.
5. **The time zone is set with `Set-TimeZone`** rather than `tzutil`. Windows
   PowerShell's quoting of arguments to programs mangles some values; a
   cmdlet parameter does not.
6. **Sleep calls `SetSuspendState` directly.** Going through
   `rundll32 powrprof.dll,SetSuspendState` passes its arguments wrongly and
   hibernates where hibernation is enabled. The client enables
   `SeShutdownPrivilege` first, which even LocalSystem holds only disabled.
   Behaviour on Modern Standby hardware is unverified.
7. **Unsupported power is detected before success is reported**
   (`check_power`). The result of a `power` job is sent before the action
   runs, so afterwards a failure can only be logged. A platform without power
   support fails the job as `unsupported`. On Windows the check confirms that
   `shutdown.exe` exists; sleep has no such check, so a failure of
   `SetSuspendState` is only logged.
8. **Cancellation has a point of no return per job kind.** A script has none:
   a cancel stops it wherever it is, with its process tree. `app.launch`,
   `setting.apply` and `power` commit just before they act (starting the
   process, applying the setting, reporting success), and `wifi.join` commits
   once it holds the Wi-Fi lock, before it joins the network. A cancel that
   arrives before that point stops the job; one that arrives later is
   ignored and the job reports its own result. Stopping a setting half way
   could leave it half applied, and a network change is not undone.
9. **A cancel for a target the client never received is recorded and reported
   as cancelled.** If a `job` message was lost, the coordinator would
   otherwise wait for ever for a device that has nothing to stop. The client
   stores the cancel like any result, so the job does not run if it arrives
   later. The coordinator also repeats `job.cancel` to a device that
   reconnects, for its targets cancelled in the last 24 hours; the device
   ignores ids it has finished and settles the rest.
10. **Built-in scripts are upserted by fixed id at start.** A newer
    coordinator refreshes the name and body of the stored copy, and
    `updated_at` moves only when something changed. A built-in whose name a
    user script already holds (names compare ignoring ASCII case) is skipped
    with a warning rather than failing the start.
11. **Only the physical console session counts as "the logged-in user".**
    Remote Desktop sessions are ignored: the device is used at its own screen.
    With nobody logged in at the console, user jobs fail as
    `no_user_session`.
