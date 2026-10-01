// Types of the coordinator HTTP API, mirroring docs/api.md. Keep the two in
// step: this file is the only place the UI describes the wire format.

export type AuthMode = 'accounts' | 'token'

export type Capabilities = {
  /** Account management available (accounts mode). */
  accounts: boolean
  /** Running inside the manager with hotspot control. */
  hotspot: boolean
}

export type Session = {
  authenticated: boolean
  auth_mode: AuthMode
  /** null in token mode. */
  account: { id: string; username: string } | null
  capabilities: Capabilities
  version: string
  protocol_version: number
}

export type Account = { id: string; username: string; created_at: number }

/** What the client reported in its last handshake; both null if it reported nothing. */
export type DeviceNetwork = { ssid: string | null; ip: string | null }

export type Device = {
  id: string
  hostname: string
  os: string
  client_version: string
  enrollment_key_id: string
  online: boolean
  first_seen_at: number
  last_seen_at: number
  /** Optional: coordinators older than milestone 3 do not send it. */
  network?: DeviceNetwork
}

export type TargetState = 'queued' | 'sent' | 'running' | 'succeeded' | 'failed' | 'expired'

export type Target = {
  id: string
  device_id: string
  state: TargetState
  exit_code: number | null
  /** unsupported, timeout, nonzero_exit, invalid_payload, interrupted, error */
  reason: string | null
  message: string | null
  /** stdout and stderr interleaved; absent in listings and events. */
  output?: string
  output_truncated: boolean
  created_at: number
  started_at: number | null
  finished_at: number | null
}

export type DeviceTarget = Target & { job: { id: string; kind: string; created_at: number } }

export type Shell = 'powershell' | 'cmd'
export type RunAs = 'system' | 'user'

export type ScriptRun = {
  shell: Shell
  body: string
  /** default 300, 1..86400 */
  timeout_secs?: number
  /** default 'system' */
  run_as?: RunAs
}

export type WifiJoin = {
  /** 1..32 bytes */
  ssid: string
  /** 8..63 characters; omitted for an open network. Returned as "********". */
  password?: string
  /** ws:// or wss://; the coordinator to use from then on */
  coordinator_url?: string
}

/**
 * Which devices a job goes to. Later milestones add device groups as a third
 * variant; everything that handles targets switches on `type`.
 */
export type TargetSpec = { type: 'all' } | { type: 'devices'; device_ids: string[] }

/** One variant per job kind the UI can create. */
export type CreateJob =
  | { kind: 'script.run'; payload: ScriptRun; targets: TargetSpec }
  | { kind: 'wifi.join'; payload: WifiJoin; targets: TargetSpec }

export type Job = {
  id: string
  kind: string
  payload: unknown
  /** username, or null in token mode */
  created_by: string | null
  created_at: number
  targets: Target[]
}

export type Key = {
  id: string
  label: string
  created_at: number
  revoked_at: number | null
  device_count: number
}

export type KeyWithSecret = Key & { secret: string }

export type Rejected = {
  id: number
  at: number
  /** remote address */
  peer: string
  device_id: string | null
  hostname: string | null
  key_id: string | null
  /** human-readable */
  reason: string
}

export type JobOutputEvent = {
  type: 'job.output'
  job_id: string
  job_target_id: string
  data: string
  truncated: boolean
  /**
   * Length in Unicode code points of the target's stored output before this
   * event's data was appended. Absent from coordinators older than milestone 3.
   */
  offset?: number
}

export type HotspotControl = 'automatic' | 'manual' | 'unsupported'
export type HotspotState = 'off' | 'starting' | 'on' | 'error'

/** The manager's hotspot (only when `capabilities.hotspot`). */
export type Hotspot = {
  /** manual: the operator enables it by hand (macOS) */
  control: HotspotControl
  state: HotspotState
  /** enrollment key the hotspot was started for */
  key_id: string | null
  /** "SC-" + first 8 characters of the key id */
  ssid: string | null
  /** derived from the key's secret */
  passphrase: string | null
  /** port devices connect to on this computer */
  client_port: number
  /** why the last start failed */
  error: string | null
  /** steps for the operator when control is 'manual' */
  instructions: string[]
}

export type ServerEvent =
  | { type: 'device.updated'; device: Device }
  | { type: 'device.removed'; device_id: string }
  | { type: 'job.created'; job: Job }
  | { type: 'job.target.updated'; job_id: string; target: Target }
  | JobOutputEvent
  | { type: 'connection.rejected'; rejected: Rejected }
  | { type: 'resync' }
