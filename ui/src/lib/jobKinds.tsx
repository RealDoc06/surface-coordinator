// Everything that depends on the kind of a job lives here, behind switches on
// `job.kind`. A new kind (app.launch, setting.apply, power, client.update) adds a case to each function; unknown kinds already render
// generically, so an older UI keeps working against a newer coordinator.

import type { ReactNode } from 'react'
import type { Job, ScriptRun } from '../api/types'
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

export function jobKindLabel(kind: string): string {
  switch (kind) {
    case 'script.run':
      return 'Script'
    case 'wifi.join':
      return 'Wi-Fi'
    default:
      return kind
  }
}

function firstLine(text: string): string {
  const line = text.split(/\r?\n/).find((l) => l.trim() !== '') ?? ''
  return line.trim()
}

/** One line that tells jobs apart in a list. */
export function jobPreview(job: Pick<Job, 'kind' | 'payload'>): string {
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
  }
  try {
    return JSON.stringify(job.payload) ?? ''
  } catch {
    return ''
  }
}

/** The job's parameters, in full, for the job detail page. */
export function JobPayload({ job }: { job: Pick<Job, 'kind' | 'payload'> }): ReactNode {
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
  }
  return (
    <pre className="code" aria-label="Payload">
      {JSON.stringify(job.payload, null, 2)}
    </pre>
  )
}

/** Where "Run again" goes, and what to open that page with. */
export type Rerun = { path: string; state: ComposeRequest }

/**
 * How to run this job again, or null when the UI has no form for its kind.
 * A wifi.join job comes back with the form filled in except for the password:
 * the coordinator only returns `********`, so it has to be entered again.
 */
export function rerunRequest(job: Job): Rerun | null {
  const targets = { type: 'devices' as const, device_ids: [...new Set(job.targets.map((t) => t.device_id))] }
  switch (job.kind) {
    case 'script.run': {
      const payload = asScriptRun(job.payload)
      if (!payload) return null
      return { path: '/run', state: { payload, targets } }
    }
    case 'wifi.join': {
      const wifi = asWifiJoin(job.payload)
      if (!wifi) return null
      const prefill: NonNullable<ComposeRequest['wifi']> = { ssid: wifi.ssid }
      if (wifi.coordinator_url) prefill.coordinator_url = wifi.coordinator_url
      if (wifi.password !== undefined) prefill.passwordNeeded = true
      return { path: '/wifi', state: { wifi: prefill, targets } }
    }
    default:
      return null
  }
}
