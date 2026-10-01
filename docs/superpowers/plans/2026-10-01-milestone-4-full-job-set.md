# Milestone 4 — Full job set: implementation plan

Date: 2026-10-01
Spec: `docs/superpowers/specs/2026-10-01-surface-coordinator-design.md`
(sections "Jobs", "Settings catalogue", "Client", "Data model")
API contract: `docs/api.md`, section "Milestone 4 additions"

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
