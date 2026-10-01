// Application state and the pure functions that change it. Everything the
// live views show comes from here: HTTP responses are loaded into it and
// WebSocket events are applied to it by `applyEvent`. No I/O in this file.

import type { ConnectionStatus } from '../api/events'
import type {
  Device,
  DeviceTarget,
  Job,
  JobOutputEvent,
  Key,
  Rejected,
  ServerEvent,
  Target,
  TargetState,
} from '../api/types'

export type OutputState = { text: string; truncated: boolean }

export type State = {
  connection: ConnectionStatus
  /**
   * Bumped whenever the views must refetch what they show: when the events
   * socket (re)connects and when the server says events were dropped.
   */
  syncEpoch: number

  devices: Record<string, Device>
  devicesLoaded: boolean
  keys: Key[] | null
  /**
   * Device and rejected-connection events received while those lists are
   * being fetched (null when no fetch is in flight). The response may predate
   * them, so they are applied again on top of it.
   */
  baseBuffer: ServerEvent[] | null

  /** Jobs by id, targets without output. */
  jobs: Record<string, Job>
  /** How much of the newest-first job list has been paged in. */
  jobList: { loaded: boolean; cursor: number | null; hasMore: boolean }

  /** Output by job target id, only for jobs whose detail is being viewed. */
  outputs: Record<string, OutputState>
  /** Output events received while a job detail request is in flight, by job id. */
  pendingOutput: Record<string, { requests: number; events: JobOutputEvent[] }>

  /** Recent targets by device id, only for devices whose detail is being viewed. */
  deviceTargets: Record<string, DeviceTarget[]>

  rejected: Rejected[]
  rejectedLoaded: boolean
  /** Rejected connections that arrived since the user last looked at them. */
  rejectedUnseen: number
}

export const initialState: State = {
  connection: 'connecting',
  syncEpoch: 0,
  devices: {},
  devicesLoaded: false,
  keys: null,
  baseBuffer: null,
  jobs: {},
  jobList: { loaded: false, cursor: null, hasMore: false },
  outputs: {},
  pendingOutput: {},
  deviceTargets: {},
  rejected: [],
  rejectedLoaded: false,
  rejectedUnseen: 0,
}

export type Action =
  | { type: 'event'; event: unknown }
  | { type: 'connection'; status: ConnectionStatus }
  | { type: 'sync' }
  | { type: 'base.requested' }
  | { type: 'base.loaded'; devices: Device[]; keys: Key[]; rejected: Rejected[] }
  | { type: 'base.failed' }
  | { type: 'devices.loaded'; devices: Device[] }
  | { type: 'keys.loaded'; keys: Key[] }
  | { type: 'rejected.loaded'; rejected: Rejected[] }
  | { type: 'rejected.seen' }
  | { type: 'jobs.loaded'; jobs: Job[]; pageSize: number; append: boolean }
  | { type: 'job.detail.requested'; jobId: string }
  | { type: 'job.detail.loaded'; job: Job }
  | { type: 'job.detail.failed'; jobId: string }
  | { type: 'job.detail.released'; jobId: string }
  | { type: 'device.detail.loaded'; device: Device; targets: DeviceTarget[] }
  | { type: 'device.detail.released'; deviceId: string }

export const MAX_REJECTED = 200
export const MAX_DEVICE_TARGETS = 50

const STATE_RANK: Record<TargetState, number> = {
  queued: 0,
  sent: 1,
  running: 2,
  succeeded: 3,
  failed: 3,
  expired: 3,
}

export function isTerminal(state: TargetState): boolean {
  return STATE_RANK[state] === 3
}

function withoutOutput(target: Target): Target {
  if (!('output' in target)) return target
  const { output: _output, ...rest } = target
  return rest
}

/**
 * A fetched target can be older than one already updated by an event that
 * overtook the response. Target states only move forward, so keep the one
 * that is further along.
 */
function freshest(fetched: Target, known: Target | undefined): Target {
  if (known && (STATE_RANK[known.state] ?? 0) > (STATE_RANK[fetched.state] ?? 0)) return known
  return fetched
}

function storeJob(jobs: Record<string, Job>, job: Job): Job {
  const known = jobs[job.id]
  return {
    ...job,
    targets: job.targets.map((t) =>
      freshest(
        withoutOutput(t),
        known?.targets.find((k) => k.id === t.id),
      ),
    ),
  }
}

/**
 * Length of a string in Unicode code points. JavaScript counts UTF-16 code
 * units, so a character outside the Basic Multilingual Plane (an emoji, say)
 * is 2 there and 1 here, which is how the coordinator counts offsets.
 */
export function codePointLength(text: string): number {
  let count = 0
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i)
    if (unit >= 0xd800 && unit <= 0xdbff && i + 1 < text.length) {
      const next = text.charCodeAt(i + 1)
      if (next >= 0xdc00 && next <= 0xdfff) i++
    }
    count++
  }
  return count
}

/**
 * Appends the data of an output event to output held as `text`, following the
 * contract: an event whose `offset` (code points of stored output before it)
 * is below what is already held is ignored, the rest is appended. Returns
 * null for an ignored event. Events without an offset (a coordinator older
 * than the offset field) are always appended; callers that can see them
 * arrive around a load use `mergeBuffered` instead.
 */
export function appendOutput(text: string, event: Pick<JobOutputEvent, 'data' | 'offset'>): string | null {
  if (event.offset !== undefined && event.offset < codePointLength(text)) return null
  return text + event.data
}

/**
 * Joins output loaded over HTTP with the output events that arrived while
 * that request was in flight.
 *
 * With offsets this is exact: events starting before the end of the loaded
 * output are already in it. Without them (an older coordinator) the ones the
 * response already contains are recognised by content: the longest run of
 * leading chunks that the stored output ends with is taken as included.
 */
export function mergeBufferedEvents(stored: string, events: Pick<JobOutputEvent, 'data' | 'offset'>[]): string {
  if (events.every((e) => e.offset !== undefined)) {
    let text = stored
    for (const event of events) text = appendOutput(text, event) ?? text
    return text
  }
  return mergeBuffered(
    stored,
    events.map((e) => e.data),
  )
}

/** The heuristic for events without offsets; see `mergeBufferedEvents`. */
export function mergeBuffered(stored: string, chunks: string[]): string {
  for (let k = chunks.length; k > 0; k--) {
    if (stored.endsWith(chunks.slice(0, k).join(''))) {
      return stored + chunks.slice(k).join('')
    }
  }
  return stored + chunks.join('')
}

function upsertDeviceTarget(list: DeviceTarget[], entry: DeviceTarget): DeviceTarget[] {
  const index = list.findIndex((t) => t.id === entry.id)
  if (index >= 0) {
    const next = list.slice()
    next[index] = entry
    return next
  }
  return [entry, ...list].slice(0, MAX_DEVICE_TARGETS)
}

const BUFFERED_TYPES: ReadonlySet<string> = new Set(['device.updated', 'device.removed', 'connection.rejected'])

function isServerEvent(raw: unknown): raw is ServerEvent {
  return typeof raw === 'object' && raw !== null && typeof (raw as { type?: unknown }).type === 'string'
}

/**
 * Applies one message from the events WebSocket. Unknown event types, and
 * anything that does not look like an event, leave the state untouched.
 */
export function applyEvent(state: State, raw: unknown): State {
  if (!isServerEvent(raw)) return state
  try {
    const next = applyKnownEvent(state, raw)
    if (state.baseBuffer && BUFFERED_TYPES.has(raw.type)) {
      return { ...next, baseBuffer: [...state.baseBuffer, raw] }
    }
    return next
  } catch {
    // A malformed event of a known type must not take the UI down.
    return state
  }
}

function applyKnownEvent(state: State, event: ServerEvent): State {
  switch (event.type) {
    case 'device.updated':
      return { ...state, devices: { ...state.devices, [event.device.id]: event.device } }

    case 'device.removed': {
      const id = event.device_id
      const devices = { ...state.devices }
      delete devices[id]
      const deviceTargets = { ...state.deviceTargets }
      delete deviceTargets[id]
      // The coordinator forgets the device's job history with it, and a job
      // left without any device is gone from its lists altogether.
      const jobs: Record<string, Job> = {}
      for (const [jobId, job] of Object.entries(state.jobs)) {
        if (!job.targets.some((t) => t.device_id === id)) {
          jobs[jobId] = job
          continue
        }
        const targets = job.targets.filter((t) => t.device_id !== id)
        if (targets.length > 0) jobs[jobId] = { ...job, targets }
      }
      return { ...state, devices, deviceTargets, jobs }
    }

    case 'job.created': {
      const job = storeJob(state.jobs, event.job)
      let deviceTargets = state.deviceTargets
      for (const target of job.targets) {
        const list = deviceTargets[target.device_id]
        if (!list) continue
        deviceTargets = {
          ...deviceTargets,
          [target.device_id]: upsertDeviceTarget(list, {
            ...target,
            job: { id: job.id, kind: job.kind, created_at: job.created_at },
          }),
        }
      }
      return { ...state, jobs: { ...state.jobs, [job.id]: job }, deviceTargets }
    }

    case 'job.target.updated': {
      const target = withoutOutput(event.target)
      const job = state.jobs[event.job_id]
      let jobs = state.jobs
      if (job) {
        const exists = job.targets.some((t) => t.id === target.id)
        const targets = exists
          ? job.targets.map((t) => (t.id === target.id ? target : t))
          : [...job.targets, target]
        jobs = { ...jobs, [job.id]: { ...job, targets } }
      }
      let deviceTargets = state.deviceTargets
      const list = deviceTargets[target.device_id]
      if (list) {
        const known = list.find((t) => t.id === target.id)
        const jobRef = known?.job ?? (job && { id: job.id, kind: job.kind, created_at: job.created_at })
        if (jobRef) {
          deviceTargets = {
            ...deviceTargets,
            [target.device_id]: upsertDeviceTarget(list, { ...target, job: jobRef }),
          }
        }
      }
      if (jobs === state.jobs && deviceTargets === state.deviceTargets) return state
      return { ...state, jobs, deviceTargets }
    }

    case 'job.output': {
      let next = state
      const pending = state.pendingOutput[event.job_id]
      if (pending) {
        next = {
          ...next,
          pendingOutput: {
            ...next.pendingOutput,
            [event.job_id]: { ...pending, events: [...pending.events, event] },
          },
        }
      }
      const current = state.outputs[event.job_target_id]
      if (current) {
        const text = appendOutput(current.text, event)
        // An event that starts before the end of what is held is already in it.
        if (text !== null) {
          next = {
            ...next,
            outputs: {
              ...next.outputs,
              [event.job_target_id]: { text, truncated: current.truncated || event.truncated },
            },
          }
        }
      }
      return next
    }

    case 'connection.rejected': {
      if (state.rejected.some((r) => r.id === event.rejected.id)) return state
      return {
        ...state,
        rejected: [event.rejected, ...state.rejected].slice(0, MAX_REJECTED),
        rejectedUnseen: state.rejectedUnseen + 1,
      }
    }

    case 'resync':
      return { ...state, syncEpoch: state.syncEpoch + 1 }

    default:
      return state
  }
}

function finishDetailRequest(state: State, jobId: string): State['pendingOutput'] {
  const pending = state.pendingOutput[jobId]
  if (!pending) return state.pendingOutput
  const next = { ...state.pendingOutput }
  if (pending.requests <= 1) delete next[jobId]
  else next[jobId] = { ...pending, requests: pending.requests - 1 }
  return next
}

export function reducer(state: State, action: Action): State {
  switch (action.type) {
    case 'event':
      return applyEvent(state, action.event)

    case 'connection':
      return {
        ...state,
        connection: action.status,
        // Events may have been missed while the socket was down.
        syncEpoch: action.status === 'open' ? state.syncEpoch + 1 : state.syncEpoch,
      }

    case 'sync':
      return { ...state, syncEpoch: state.syncEpoch + 1 }

    case 'base.requested':
      return state.baseBuffer ? state : { ...state, baseBuffer: [] }

    case 'base.failed':
      return { ...state, baseBuffer: null }

    case 'base.loaded': {
      const buffered = state.baseBuffer ?? []
      let next: State = { ...state, baseBuffer: null }
      next = reducer(next, { type: 'devices.loaded', devices: action.devices })
      next = reducer(next, { type: 'keys.loaded', keys: action.keys })
      next = reducer(next, { type: 'rejected.loaded', rejected: action.rejected })
      // Events that crossed the request were already counted as unseen once.
      const unseen = next.rejectedUnseen
      for (const event of buffered) next = applyEvent(next, event)
      return { ...next, rejectedUnseen: unseen }
    }

    case 'devices.loaded': {
      const devices: Record<string, Device> = {}
      for (const d of action.devices) devices[d.id] = d
      return { ...state, devices, devicesLoaded: true }
    }

    case 'keys.loaded':
      return { ...state, keys: action.keys }

    case 'rejected.loaded': {
      let unseen = state.rejectedUnseen
      if (state.rejectedLoaded) {
        const known = new Set(state.rejected.map((r) => r.id))
        const newest = state.rejected.reduce((max, r) => Math.max(max, r.id), -Infinity)
        unseen += action.rejected.filter((r) => !known.has(r.id) && r.id > newest).length
      }
      return {
        ...state,
        rejected: action.rejected.slice(0, MAX_REJECTED),
        rejectedLoaded: true,
        rejectedUnseen: unseen,
      }
    }

    case 'rejected.seen':
      return state.rejectedUnseen === 0 ? state : { ...state, rejectedUnseen: 0 }

    case 'jobs.loaded': {
      const jobs = { ...state.jobs }
      for (const job of action.jobs) jobs[job.id] = storeJob(state.jobs, job)
      const oldest = action.jobs.reduce<number | null>(
        (min, j) => (min === null || j.created_at < min ? j.created_at : min),
        action.append ? state.jobList.cursor : null,
      )
      return {
        ...state,
        jobs,
        jobList: { loaded: true, cursor: oldest, hasMore: action.jobs.length >= action.pageSize },
      }
    }

    case 'job.detail.requested': {
      const pending = state.pendingOutput[action.jobId]
      return {
        ...state,
        pendingOutput: {
          ...state.pendingOutput,
          [action.jobId]: pending
            ? { ...pending, requests: pending.requests + 1 }
            : { requests: 1, events: [] },
        },
      }
    }

    case 'job.detail.loaded': {
      const job = action.job
      const jobs = { ...state.jobs, [job.id]: storeJob(state.jobs, job) }
      const pending = state.pendingOutput[job.id]
      // Not requested, or the view was left before the response came.
      if (!pending) return { ...state, jobs }
      const outputs = { ...state.outputs }
      for (const target of job.targets) {
        const chunks = pending.events.filter((e) => e.job_target_id === target.id)
        outputs[target.id] = {
          text: mergeBufferedEvents(target.output ?? '', chunks),
          truncated: target.output_truncated || chunks.some((c) => c.truncated),
        }
      }
      return { ...state, jobs, outputs, pendingOutput: finishDetailRequest(state, job.id) }
    }

    case 'job.detail.failed':
      return { ...state, pendingOutput: finishDetailRequest(state, action.jobId) }

    case 'job.detail.released': {
      const job = state.jobs[action.jobId]
      const outputs = { ...state.outputs }
      for (const target of job?.targets ?? []) delete outputs[target.id]
      const pendingOutput = { ...state.pendingOutput }
      delete pendingOutput[action.jobId]
      return { ...state, outputs, pendingOutput }
    }

    case 'device.detail.loaded': {
      const id = action.device.id
      // Jobs known from events can be ahead of the response, or missing from it.
      let targets = action.targets.map((fetched): DeviceTarget => {
        const known = state.jobs[fetched.job.id]?.targets.find((t) => t.id === fetched.id)
        return { ...freshest(withoutOutput(fetched), known), job: fetched.job }
      })
      const newest = targets.reduce((max, t) => Math.max(max, t.job.created_at), -Infinity)
      const have = new Set(targets.map((t) => t.id))
      for (const job of Object.values(state.jobs)) {
        if (job.created_at <= newest) continue
        for (const target of job.targets) {
          if (target.device_id !== id || have.has(target.id)) continue
          targets.push({ ...target, job: { id: job.id, kind: job.kind, created_at: job.created_at } })
        }
      }
      targets = targets.sort((a, b) => b.job.created_at - a.job.created_at).slice(0, MAX_DEVICE_TARGETS)
      return {
        ...state,
        // The device list is the authority on the device; only fill a gap.
        devices: state.devices[id] ? state.devices : { ...state.devices, [id]: action.device },
        deviceTargets: { ...state.deviceTargets, [id]: targets },
      }
    }

    case 'device.detail.released': {
      const deviceTargets = { ...state.deviceTargets }
      delete deviceTargets[action.deviceId]
      return { ...state, deviceTargets }
    }

    default:
      return state
  }
}

// Selectors

export function sortedDevices(state: State): Device[] {
  return Object.values(state.devices).sort(
    (a, b) =>
      a.hostname.localeCompare(b.hostname, undefined, { sensitivity: 'base', numeric: true }) ||
      a.id.localeCompare(b.id),
  )
}

/** The contiguous, newest-first part of the job list that has been loaded. */
export function listedJobs(state: State): Job[] {
  const { cursor, hasMore } = state.jobList
  return Object.values(state.jobs)
    .filter((job) => !hasMore || cursor === null || job.created_at >= cursor)
    .sort((a, b) => b.created_at - a.created_at || b.id.localeCompare(a.id))
}

export function keyLabels(state: State): Record<string, string> {
  const labels: Record<string, string> = {}
  for (const key of state.keys ?? []) labels[key.id] = key.label
  return labels
}
