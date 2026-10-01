// Target selection as data: which devices a job goes to, and how the
// composer is told what to open with. Device groups (a later milestone) add a
// variant to TargetSpec and a case to each switch here.

import type { Device, ScriptRun, TargetSpec } from '../api/types'

/** What the Wi-Fi form starts from (`Run again`); the password is never known. */
export type WifiPrefill = {
  ssid?: string
  coordinator_url?: string
  /** The job being repeated had a password, which the server does not give back. */
  passwordNeeded?: boolean
}

/** What a page passes to a job form (composer, Wi-Fi) through router state. */
export type ComposeRequest = {
  targets?: TargetSpec
  payload?: Partial<ScriptRun>
  wifi?: WifiPrefill
}

export type ResolvedTargets = {
  /** Devices the job would go to that are known to the UI. */
  devices: Device[]
  /** Selected ids that no longer match a device. */
  missing: string[]
  offline: number
}

export function resolveTargets(spec: TargetSpec, devices: Device[]): ResolvedTargets {
  switch (spec.type) {
    case 'all':
      return { devices, missing: [], offline: devices.filter((d) => !d.online).length }
    case 'devices': {
      const byId = new Map(devices.map((d) => [d.id, d]))
      const found: Device[] = []
      const missing: string[] = []
      for (const id of spec.device_ids) {
        const device = byId.get(id)
        if (device) found.push(device)
        else missing.push(id)
      }
      return { devices: found, missing, offline: found.filter((d) => !d.online).length }
    }
  }
}

export function targetCount(spec: TargetSpec, devices: Device[]): number {
  return spec.type === 'all' ? devices.length : spec.device_ids.length
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((v) => typeof v === 'string')
}

export function parseTargetSpec(value: unknown): TargetSpec | null {
  if (typeof value !== 'object' || value === null) return null
  const spec = value as { type?: unknown; device_ids?: unknown }
  if (spec.type === 'all') return { type: 'all' }
  if (spec.type === 'devices' && isStringArray(spec.device_ids)) {
    return { type: 'devices', device_ids: [...new Set(spec.device_ids)] }
  }
  return null
}

/** Router state is untyped and survives reloads; read it defensively. */
export function parseComposeRequest(value: unknown): ComposeRequest {
  if (typeof value !== 'object' || value === null) return {}
  const raw = value as { targets?: unknown; payload?: unknown; wifi?: unknown }
  const request: ComposeRequest = {}
  const targets = parseTargetSpec(raw.targets)
  if (targets) request.targets = targets
  if (typeof raw.payload === 'object' && raw.payload !== null) {
    const p = raw.payload as Record<string, unknown>
    const payload: Partial<ScriptRun> = {}
    if (p.shell === 'powershell' || p.shell === 'cmd') payload.shell = p.shell
    if (typeof p.body === 'string') payload.body = p.body
    if (typeof p.timeout_secs === 'number') payload.timeout_secs = p.timeout_secs
    if (p.run_as === 'system' || p.run_as === 'user') payload.run_as = p.run_as
    request.payload = payload
  }
  if (typeof raw.wifi === 'object' && raw.wifi !== null) {
    const w = raw.wifi as Record<string, unknown>
    const wifi: WifiPrefill = {}
    if (typeof w.ssid === 'string') wifi.ssid = w.ssid
    if (typeof w.coordinator_url === 'string') wifi.coordinator_url = w.coordinator_url
    if (w.passwordNeeded === true) wifi.passwordNeeded = true
    request.wifi = wifi
  }
  return request
}
