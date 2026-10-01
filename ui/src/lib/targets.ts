// Target selection as data: which devices a job goes to, and how the job forms
// are told what to open with (router state).

import type { Device, Group, PowerAction, ScriptRun, TargetSpec } from '../api/types'

/** What the Wi-Fi form starts from (`Run again`); the password is never known. */
export type WifiPrefill = {
  ssid?: string
  coordinator_url?: string
  /** The job being repeated had a password, which the server does not give back. */
  passwordNeeded?: boolean
}

export type LaunchPrefill = { path?: string; arguments?: string[] }
export type SettingPrefill = { setting?: string; value?: boolean | number | string }
export type PowerPrefill = { action?: PowerAction }
export type UpdatePrefill = { version?: string; downgrade?: boolean }

/** What a page passes to a job form (composer, Wi-Fi, ...) through router state. */
export type ComposeRequest = {
  targets?: TargetSpec
  payload?: Partial<ScriptRun>
  wifi?: WifiPrefill
  launch?: LaunchPrefill
  setting?: SettingPrefill
  power?: PowerPrefill
  update?: UpdatePrefill
}

export type ResolvedTargets = {
  /** Devices the job would go to that are known to the UI. */
  devices: Device[]
  /** Selected ids that no longer match a device. */
  missing: string[]
  offline: number
  /** For a group target: the group, or null when it no longer exists. Undefined for other targets. */
  group?: Group | null
}

export function resolveTargets(spec: TargetSpec, devices: Device[], groups: Group[] = []): ResolvedTargets {
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
    case 'group': {
      const group = groups.find((g) => g.id === spec.group_id) ?? null
      if (!group) return { devices: [], missing: [], offline: 0, group: null }
      const members = new Set(group.device_ids)
      const found = devices.filter((d) => members.has(d.id))
      return { devices: found, missing: [], offline: found.filter((d) => !d.online).length, group }
    }
  }
}

/** What to send as `targets`: a device list is reduced to the devices that still exist. */
export function requestTargets(spec: TargetSpec, resolved: ResolvedTargets): TargetSpec {
  return spec.type === 'devices' ? { type: 'devices', device_ids: resolved.devices.map((d) => d.id) } : spec
}

/** The complaint when a form is submitted with nothing to run on. `verb` completes "There are no devices to ...". */
export function noTargetsMessage(spec: TargetSpec | null, verb: string): string {
  switch (spec?.type) {
    case 'all':
      return `There are no devices to ${verb} yet.`
    case 'group':
      return 'That group has no devices (or no longer exists).'
    default:
      return `Select at least one device to ${verb}.`
  }
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((v) => typeof v === 'string')
}

export function parseTargetSpec(value: unknown): TargetSpec | null {
  if (typeof value !== 'object' || value === null) return null
  const spec = value as { type?: unknown; device_ids?: unknown; group_id?: unknown }
  if (spec.type === 'all') return { type: 'all' }
  if (spec.type === 'devices' && isStringArray(spec.device_ids)) {
    return { type: 'devices', device_ids: [...new Set(spec.device_ids)] }
  }
  if (spec.type === 'group' && typeof spec.group_id === 'string' && spec.group_id !== '') {
    return { type: 'group', group_id: spec.group_id }
  }
  return null
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null ? (value as Record<string, unknown>) : null
}

/** Router state is untyped and survives reloads; read it defensively. */
export function parseComposeRequest(value: unknown): ComposeRequest {
  const raw = record(value)
  if (!raw) return {}
  const request: ComposeRequest = {}
  const targets = parseTargetSpec(raw.targets)
  if (targets) request.targets = targets

  const p = record(raw.payload)
  if (p) {
    const payload: Partial<ScriptRun> = {}
    if (p.shell === 'powershell' || p.shell === 'cmd') payload.shell = p.shell
    if (typeof p.body === 'string') payload.body = p.body
    if (typeof p.timeout_secs === 'number') payload.timeout_secs = p.timeout_secs
    if (p.run_as === 'system' || p.run_as === 'user') payload.run_as = p.run_as
    request.payload = payload
  }

  const w = record(raw.wifi)
  if (w) {
    const wifi: WifiPrefill = {}
    if (typeof w.ssid === 'string') wifi.ssid = w.ssid
    if (typeof w.coordinator_url === 'string') wifi.coordinator_url = w.coordinator_url
    if (w.passwordNeeded === true) wifi.passwordNeeded = true
    request.wifi = wifi
  }

  const l = record(raw.launch)
  if (l) {
    const launch: LaunchPrefill = {}
    if (typeof l.path === 'string') launch.path = l.path
    if (isStringArray(l.arguments)) launch.arguments = l.arguments
    request.launch = launch
  }

  const s = record(raw.setting)
  if (s) {
    const setting: SettingPrefill = {}
    if (typeof s.setting === 'string') setting.setting = s.setting
    if (typeof s.value === 'boolean' || typeof s.value === 'number' || typeof s.value === 'string') {
      setting.value = s.value
    }
    request.setting = setting
  }

  const pw = record(raw.power)
  if (pw) {
    const power: PowerPrefill = {}
    if (pw.action === 'restart' || pw.action === 'shutdown' || pw.action === 'sleep') power.action = pw.action
    request.power = power
  }

  const u = record(raw.update)
  if (u) {
    const update: UpdatePrefill = {}
    if (typeof u.version === 'string') update.version = u.version
    if (u.downgrade === true) update.downgrade = true
    request.update = update
  }
  return request
}
