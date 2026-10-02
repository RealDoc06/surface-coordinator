// The one module that talks HTTP to the coordinator. Every call is a
// same-origin relative URL, so the UI works unchanged whether it is served by
// a remote coordinator or by the one embedded in the manager.

import type {
  Account,
  CreateJob,
  Device,
  DeviceTarget,
  Group,
  Hotspot,
  ManagerWifi,
  TargetSpec,
  Job,
  Key,
  KeyWithSecret,
  Rejected,
  Script,
  ScriptInput,
  Session,
  Setting,
  Updates,
} from './types'

export const API_BASE = '/api/v1'
export const JOBS_PAGE_SIZE = 50

export class ApiError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
}

let unauthorizedHandler: (() => void) | null = null

/** Called when any request is answered with `401 unauthorized` (session gone). */
export function setUnauthorizedHandler(handler: (() => void) | null): void {
  unauthorizedHandler = handler
}

async function readError(res: Response): Promise<ApiError> {
  let code = `http_${res.status}`
  let message = `The coordinator answered with status ${res.status}.`
  try {
    const body: unknown = await res.json()
    if (typeof body === 'object' && body !== null && 'error' in body) {
      const err = (body as { error: unknown }).error
      if (typeof err === 'object' && err !== null) {
        const e = err as { code?: unknown; message?: unknown }
        if (typeof e.code === 'string') code = e.code
        if (typeof e.message === 'string' && e.message !== '') message = e.message
      }
    }
  } catch {
    // Not JSON (a proxy error page, for instance): keep the generic message.
  }
  return new ApiError(res.status, code, message)
}

function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const init: RequestInit = {
    method,
    credentials: 'same-origin',
    headers:
      body === undefined
        ? { Accept: 'application/json' }
        : { Accept: 'application/json', 'Content-Type': 'application/json' },
  }
  if (body !== undefined) init.body = JSON.stringify(body)
  return send<T>(path, init)
}

async function send<T>(path: string, init: RequestInit): Promise<T> {
  let res: Response
  try {
    res = await fetch(`${API_BASE}${path}`, init)
  } catch {
    throw new ApiError(0, 'network_error', 'Could not reach the coordinator.')
  }
  if (!res.ok) {
    const err = await readError(res)
    if (res.status === 401 && err.code === 'unauthorized') unauthorizedHandler?.()
    throw err
  }
  if (res.status === 204) return undefined as T
  try {
    return (await res.json()) as T
  } catch {
    throw new ApiError(res.status, 'bad_response', 'The coordinator sent a response that is not JSON.')
  }
}

const seg = encodeURIComponent

export type ClientUpload = {
  /** The signed client executable; sent as the request body without reading it into memory. */
  file: File
  /** "1.2.3" */
  version: string
  /** Base64 signature over the file, version and target. */
  signature: string
  notes?: string
}

/** The one target the UI uploads builds for. */
export const UPLOAD_TARGET = 'windows-x86_64'

function uploadClient({ file, version, signature, notes }: ClientUpload) {
  const params = new URLSearchParams({ version, target: UPLOAD_TARGET })
  if (notes) params.set('notes', notes)
  return send<Updates>(`/updates/upload?${params.toString()}`, {
    method: 'POST',
    credentials: 'same-origin',
    headers: { Accept: 'application/json', 'Content-Type': 'application/octet-stream', 'X-Signature': signature },
    body: file,
  })
}

export const api = {
  // Session and authentication
  getSession: () => request<Session>('GET', '/session'),
  login: (username: string, password: string) =>
    request<{ account: Account }>('POST', '/auth/login', { username, password }),
  exchangeToken: (token: string) => request<void>('POST', '/auth/token', { token }),
  logout: () => request<void>('POST', '/auth/logout'),

  // Accounts
  listAccounts: () => request<{ accounts: Account[] }>('GET', '/accounts'),
  createAccount: (username: string, password: string) =>
    request<{ account: Account }>('POST', '/accounts', { username, password }),
  setAccountPassword: (id: string, password: string) =>
    request<void>('POST', `/accounts/${seg(id)}/password`, { password }),
  deleteAccount: (id: string) => request<void>('DELETE', `/accounts/${seg(id)}`),

  // Devices
  listDevices: () => request<{ devices: Device[] }>('GET', '/devices'),
  getDevice: (id: string) =>
    request<{ device: Device; targets: DeviceTarget[] }>('GET', `/devices/${seg(id)}`),
  forgetDevice: (id: string) => request<void>('DELETE', `/devices/${seg(id)}`),

  // Jobs
  createJob: (job: CreateJob) => request<{ job: Job }>('POST', '/jobs', job),
  listJobs: (opts: { limit?: number; before?: number } = {}) => {
    const params = new URLSearchParams()
    params.set('limit', String(opts.limit ?? JOBS_PAGE_SIZE))
    if (opts.before !== undefined) params.set('before', String(opts.before))
    return request<{ jobs: Job[] }>('GET', `/jobs?${params.toString()}`)
  },
  getJob: (id: string) => request<{ job: Job }>('GET', `/jobs/${seg(id)}`),
  cancelJob: (id: string) => request<{ job: Job }>('POST', `/jobs/${seg(id)}/cancel`),

  // Settings catalogue
  getSettingsCatalogue: () => request<{ settings: Setting[] }>('GET', '/settings-catalogue'),

  // Groups
  listGroups: () => request<{ groups: Group[] }>('GET', '/groups'),
  createGroup: (name: string) => request<{ group: Group }>('POST', '/groups', { name }),
  renameGroup: (id: string, name: string) => request<{ group: Group }>('PATCH', `/groups/${seg(id)}`, { name }),
  deleteGroup: (id: string) => request<void>('DELETE', `/groups/${seg(id)}`),
  /** Replaces the whole membership. */
  setGroupMembers: (id: string, deviceIds: string[]) =>
    request<{ group: Group }>('PUT', `/groups/${seg(id)}/members`, { device_ids: deviceIds }),

  // Script library
  listScripts: () => request<{ scripts: Script[] }>('GET', '/scripts'),
  createScript: (script: ScriptInput) => request<{ script: Script }>('POST', '/scripts', script),
  updateScript: (id: string, script: ScriptInput) =>
    request<{ script: Script }>('PUT', `/scripts/${seg(id)}`, script),
  deleteScript: (id: string) => request<void>('DELETE', `/scripts/${seg(id)}`),

  // Client updates
  getUpdates: () => request<Updates>('GET', '/updates'),
  checkUpdates: () => request<Updates>('POST', '/updates/check'),
  /** Adds a client build signed with the update key to the cache. */
  uploadClient,
  setAutoUpdate: (enabled: boolean) => request<Updates>('POST', '/updates/auto', { enabled }),

  // Enrollment keys
  listKeys: () => request<{ keys: Key[] }>('GET', '/enrollment-keys'),
  createKey: (label: string) => request<{ key: KeyWithSecret }>('POST', '/enrollment-keys', { label }),
  importKey: (key: { label: string; id: string; secret: string }) =>
    request<{ key: KeyWithSecret }>('POST', '/enrollment-keys', key),
  exportKeySecret: (id: string) =>
    request<{ id: string; secret: string }>('GET', `/enrollment-keys/${seg(id)}/secret`),
  revokeKey: (id: string) => request<void>('POST', `/enrollment-keys/${seg(id)}/revoke`),

  // Hotspot (served by the manager; only when capabilities.hotspot)
  getHotspot: () => request<Hotspot>('GET', '/manager/hotspot'),
  startHotspot: (keyId: string) => request<Hotspot>('POST', '/manager/hotspot/start', { key_id: keyId }),
  stopHotspot: () => request<Hotspot>('POST', '/manager/hotspot/stop'),
  /** The network the manager's computer is on; the password stays with the manager. */
  getManagerWifi: () => request<ManagerWifi>('GET', '/manager/wifi'),
  /** Moves devices to that network; the manager fills in SSID, password and coordinator address. */
  moveToManagerWifi: (targets: TargetSpec) => request<{ job: Job }>('POST', '/manager/wifi/move', { targets }),

  // Rejected connection attempts
  listRejected: () => request<{ rejected: Rejected[] }>('GET', '/rejected-connections'),
}

const MESSAGES: Record<string, string> = {
  invalid_credentials: 'Wrong username or password.',
  too_many_attempts: 'Too many failed attempts. Wait half a minute and try again.',
  wrong_auth_mode: 'This coordinator does not accept that kind of sign-in.',
  invalid_token: 'The manager token was not accepted. Reopen this page from the manager.',
  cross_origin: 'The request was refused because it did not come from the coordinator’s own page.',
  invalid_username: 'Usernames are 1–64 characters: letters, digits, dot, underscore and hyphen.',
  weak_password: 'Passwords need at least 10 characters.',
  username_taken: 'An account with that username already exists.',
  cannot_delete_self: 'You cannot delete the account you are signed in with.',
  unsupported_kind: 'The coordinator does not support this kind of job.',
  unknown_device: 'One of the selected devices no longer exists. Go back and select again.',
  no_targets: 'The job has no target devices. If you chose a group, it has no members.',
  unknown_group: 'That group no longer exists. Go back and choose again.',
  name_taken: 'Another entry already has that name. Choose a different name.',
  built_in: 'Built-in scripts cannot be changed or deleted. Duplicate it to make your own copy.',
  invalid_key: 'The key id or secret is not valid.',
  key_exists: 'An enrollment key with that id already exists.',
  key_revoked: 'That enrollment key has been revoked. Choose another one.',
  invalid_version: 'The version must have three numbers, like 1.2.3.',
  unsupported_target: 'This coordinator does not take client builds for that platform.',
  bad_signature: "The signature does not match this file, version and the coordinator's update key.",
  version_exists: 'That version has already been uploaded. Use a new version number.',
  too_large: 'The file is too large for this coordinator.',
  http_413: 'The file is too large for this coordinator, or for a proxy in front of it.',
  updates_unavailable: 'This coordinator has no update key built in, so it cannot take client builds.',
  unauthorized: 'You are no longer signed in.',
  network_error: 'Could not reach the coordinator.',
}

/** A message for people, for anything a request can throw. */
export function errorMessage(err: unknown): string {
  if (err instanceof ApiError) return MESSAGES[err.code] ?? err.message
  if (err instanceof Error) return err.message
  return 'Something went wrong.'
}
