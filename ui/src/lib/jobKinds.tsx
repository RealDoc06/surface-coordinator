// Everything that depends on the kind of a job lives here, behind switches on
// `job.kind`. A new kind adds a case to each function; unknown kinds already
// render generically, so an older UI keeps working against a newer coordinator.

import type { ReactNode } from 'react'
import type {
  AppLaunch,
  ClientUpdate,
  Job,
  Power,
  PowerAction,
  ScriptRun,
  Setting,
  SettingApply,
} from '../api/types'
import { actionFor, type ActionKind } from './actions'
import { findSetting, formatSettingValue, valueToText } from './settings'
import { asWifiJoin } from './wifi'
import type { ComposeRequest } from './targets'

export const DEFAULT_TIMEOUT_SECS = 300

export const SHELL_LABELS: Record<ScriptRun['shell'], string> = {
  powershell: 'PowerShell',
  cmd: 'cmd',
}

export const RUN_AS_LABELS: Record<NonNullable<ScriptRun['run_as']>, string> = {
  system: 'System',
  user: 'Logged-in user',
}

export const POWER_LABELS: Record<PowerAction, string> = {
  restart: 'Restart',
  shutdown: 'Shut down',
  sleep: 'Sleep',
}

/** Things from outside a job that make its description better (all optional). */
export type JobContext = {
  /** The settings catalogue, when loaded: previews use its labels. */
  settings?: Setting[] | null
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}

/** The payload of a script.run job, or null if it does not look like one. */
export function asScriptRun(payload: unknown): ScriptRun | null {
  if (typeof payload !== 'object' || payload === null) return null
  const p = payload as Record<string, unknown>
  if ((p.shell !== 'powershell' && p.shell !== 'cmd') || typeof p.body !== 'string') return null
  const script: ScriptRun = { shell: p.shell, body: p.body }
  if (typeof p.timeout_secs === 'number') script.timeout_secs = p.timeout_secs
  if (p.run_as === 'system' || p.run_as === 'user') script.run_as = p.run_as
  return script
}

export function asAppLaunch(payload: unknown): AppLaunch | null {
  if (!isRecord(payload) || typeof payload.path !== 'string') return null
  const launch: AppLaunch = { path: payload.path }
  if (Array.isArray(payload.arguments) && payload.arguments.every((a) => typeof a === 'string')) {
    launch.arguments = payload.arguments as string[]
  }
  return launch
}

export function asSettingApply(payload: unknown): SettingApply | null {
  if (!isRecord(payload) || typeof payload.setting !== 'string') return null
  const v = payload.value
  if (typeof v !== 'boolean' && typeof v !== 'number' && typeof v !== 'string') return null
  return { setting: payload.setting, value: v }
}

export function asPower(payload: unknown): Power | null {
  if (!isRecord(payload)) return null
  const a = payload.action
  return a === 'restart' || a === 'shutdown' || a === 'sleep' ? { action: a } : null
}

export function asClientUpdate(payload: unknown): ClientUpdate | null {
  if (!isRecord(payload) || typeof payload.version !== 'string') return null
  const update: ClientUpdate = { version: payload.version }
  if (payload.downgrade === true) update.downgrade = true
  return update
}

export function jobKindLabel(kind: string): string {
  switch (kind) {
    case 'script.run':
      return 'Script'
    case 'wifi.join':
      return 'Wi-Fi'
    case 'app.launch':
      return 'Launch app'
    case 'setting.apply':
      return 'Setting'
    case 'power':
      return 'Power'
    case 'client.update':
      return 'Client update'
    default:
      return kind
  }
}

function firstLine(text: string): string {
  const line = text.split(/\r?\n/).find((l) => l.trim() !== '') ?? ''
  return line.trim()
}

/** One line that tells jobs apart in a list. */
export function jobPreview(job: Pick<Job, 'kind' | 'payload'>, context: JobContext = {}): string {
  switch (job.kind) {
    case 'script.run': {
      const script = asScriptRun(job.payload)
      if (script) return firstLine(script.body)
      break
    }
    case 'wifi.join': {
      const wifi = asWifiJoin(job.payload)
      if (wifi) return `Join Wi-Fi ${wifi.ssid}`
      break
    }
    case 'app.launch': {
      const launch = asAppLaunch(job.payload)
      if (launch) return `Launch ${launch.path}`
      break
    }
    case 'setting.apply': {
      const apply = asSettingApply(job.payload)
      if (apply) {
        const setting = findSetting(context.settings, apply.setting)
        return `Set ${setting?.label ?? apply.setting} to ${formatSettingValue(setting, apply.value)}`
      }
      break
    }
    case 'power': {
      const power = asPower(job.payload)
      if (power) return POWER_LABELS[power.action]
      break
    }
    case 'client.update': {
      const update = asClientUpdate(job.payload)
      if (update) return `Update client to ${update.version}${update.downgrade ? ' (downgrade allowed)' : ''}`
      break
    }
  }
  try {
    return JSON.stringify(job.payload) ?? ''
  } catch {
    return ''
  }
}

/** The job's parameters, in full, for the job detail page. */
export function JobPayload({
  job,
  context = {},
}: {
  job: Pick<Job, 'kind' | 'payload'>
  context?: JobContext
}): ReactNode {
  switch (job.kind) {
    case 'script.run': {
      const script = asScriptRun(job.payload)
      if (script) {
        return (
          <>
            <dl className="fields">
              <div>
                <dt>Shell</dt>
                <dd>{SHELL_LABELS[script.shell]}</dd>
              </div>
              <div>
                <dt>Run as</dt>
                <dd>{RUN_AS_LABELS[script.run_as ?? 'system']}</dd>
              </div>
              <div>
                <dt>Timeout</dt>
                <dd>{script.timeout_secs ?? DEFAULT_TIMEOUT_SECS} s</dd>
              </div>
            </dl>
            <pre className="code" aria-label="Script">
              {script.body}
            </pre>
          </>
        )
      }
      break
    }
    case 'wifi.join': {
      const wifi = asWifiJoin(job.payload)
      if (wifi) {
        return (
          <dl className="fields">
            <div>
              <dt>Network (SSID)</dt>
              <dd>{wifi.ssid}</dd>
            </div>
            <div>
              <dt>Password</dt>
              <dd>
                {wifi.password === undefined ? (
                  <span className="muted">none (open network)</span>
                ) : (
                  <span className="mono" title="The coordinator does not keep the password in readable form.">
                    {wifi.password}
                  </span>
                )}
              </dd>
            </div>
            <div>
              <dt>Coordinator from then on</dt>
              <dd>{wifi.coordinator_url ? <span className="mono">{wifi.coordinator_url}</span> : <span className="muted">unchanged</span>}</dd>
            </div>
          </dl>
        )
      }
      break
    }
    case 'app.launch': {
      const launch = asAppLaunch(job.payload)
      if (launch) {
        return (
          <dl className="fields">
            <div>
              <dt>Program</dt>
              <dd className="mono">{launch.path}</dd>
            </div>
            <div>
              <dt>Arguments</dt>
              <dd>
                {launch.arguments && launch.arguments.length > 0 ? (
                  <ol className="arg-list mono">
                    {launch.arguments.map((a, i) => (
                      <li key={i}>{a}</li>
                    ))}
                  </ol>
                ) : (
                  <span className="muted">none</span>
                )}
              </dd>
            </div>
          </dl>
        )
      }
      break
    }
    case 'setting.apply': {
      const apply = asSettingApply(job.payload)
      if (apply) {
        const setting = findSetting(context.settings, apply.setting)
        return (
          <dl className="fields">
            <div>
              <dt>Setting</dt>
              <dd>
                {setting ? (
                  <>
                    {setting.label} <span className="muted mono">{setting.id}</span>
                  </>
                ) : (
                  <span className="mono">{apply.setting}</span>
                )}
              </dd>
            </div>
            <div>
              <dt>Value</dt>
              <dd>{formatSettingValue(setting, apply.value)}</dd>
            </div>
          </dl>
        )
      }
      break
    }
    case 'power': {
      const power = asPower(job.payload)
      if (power) {
        return (
          <dl className="fields">
            <div>
              <dt>Action</dt>
              <dd>{POWER_LABELS[power.action]}</dd>
            </div>
          </dl>
        )
      }
      break
    }
    case 'client.update': {
      const update = asClientUpdate(job.payload)
      if (update) {
        return (
          <dl className="fields">
            <div>
              <dt>Version</dt>
              <dd>{update.version}</dd>
            </div>
            <div>
              <dt>Downgrade</dt>
              <dd>{update.downgrade ? 'allowed' : 'not allowed'}</dd>
            </div>
          </dl>
        )
      }
      break
    }
  }
  return (
    <pre className="code" aria-label="Payload">
      {JSON.stringify(job.payload, null, 2)}
    </pre>
  )
}

/** Where "Run again" goes, and what to open that page with. */
export type Rerun = { path: string; state: ComposeRequest }

function formPath(kind: ActionKind): string {
  return actionFor(kind).path
}

/**
 * How to run this job again, or null when the UI has no form for its kind.
 * A wifi.join job comes back with the form filled in except for the password:
 * the coordinator only returns `********`, so it has to be entered again.
 */
export function rerunRequest(job: Job, context: JobContext = {}): Rerun | null {
  const targets = { type: 'devices' as const, device_ids: [...new Set(job.targets.map((t) => t.device_id))] }
  switch (job.kind) {
    case 'script.run': {
      const payload = asScriptRun(job.payload)
      if (!payload) return null
      return { path: formPath('script.run'), state: { payload, targets } }
    }
    case 'wifi.join': {
      const wifi = asWifiJoin(job.payload)
      if (!wifi) return null
      const prefill: NonNullable<ComposeRequest['wifi']> = { ssid: wifi.ssid }
      if (wifi.coordinator_url) prefill.coordinator_url = wifi.coordinator_url
      if (wifi.password !== undefined) prefill.passwordNeeded = true
      return { path: formPath('wifi.join'), state: { wifi: prefill, targets } }
    }
    case 'app.launch': {
      const launch = asAppLaunch(job.payload)
      if (!launch) return null
      return {
        path: formPath('app.launch'),
        state: { launch: { path: launch.path, arguments: launch.arguments ?? [] }, targets },
      }
    }
    case 'setting.apply': {
      const apply = asSettingApply(job.payload)
      if (!apply) return null
      // A value that no longer fits the setting's type is not carried over.
      const setting = findSetting(context.settings, apply.setting)
      const fits = !setting || valueToText(setting, apply.value) !== null
      return {
        path: formPath('setting.apply'),
        state: { setting: fits ? { setting: apply.setting, value: apply.value } : { setting: apply.setting }, targets },
      }
    }
    case 'power': {
      const power = asPower(job.payload)
      if (!power) return null
      return { path: formPath('power'), state: { power: { action: power.action }, targets } }
    }
    case 'client.update': {
      const update = asClientUpdate(job.payload)
      if (!update) return null
      return {
        path: formPath('client.update'),
        state: { update: { version: update.version, ...(update.downgrade ? { downgrade: true } : {}) }, targets },
      }
    }
    default:
      return null
  }
}
