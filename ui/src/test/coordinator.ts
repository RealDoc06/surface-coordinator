// A small in-memory coordinator behind the mocked fetch, for tests of whole
// pages. It implements the milestone 1-5 routes the UI uses, closely enough
// to follow what a screen does; `override` lets a test make a call misbehave.

import type { Device, Group, Job, Script, Session, Setting, Updates } from '../api/types'
import { apiError, device, job, mockFetch, target, type Call } from './helpers'

type Reply = { status?: number; body?: unknown }

export type FakeServer = {
  session: Session
  devices: Device[]
  groups: Group[]
  scripts: Script[]
  jobs: Record<string, Job>
  catalogue: Setting[] | null
  /** null: the coordinator answers 404 (older than milestone 5). */
  updates: Updates | null
  /** What `POST /updates/check` returns; an error reply makes the check fail. */
  checkResult: Reply | null
  calls: Call[]
  /** Answers a call instead of the built-in behaviour when it returns something. */
  override?: (call: Call) => Reply | undefined
}

export const baseSession: Session = {
  authenticated: true,
  auth_mode: 'token',
  account: null,
  capabilities: { accounts: false, hotspot: false },
  version: '0.5.0',
  protocol_version: 3,
}

export function group(id: string, name: string, deviceIds: string[] = []): Group {
  return { id, name, created_at: 1_000, device_ids: deviceIds }
}

export function script(id: string, name: string, overrides: Partial<Script> = {}): Script {
  return {
    id,
    name,
    shell: 'powershell',
    body: `Write-Output '${name}'`,
    built_in: false,
    created_at: 1_000,
    updated_at: 2_000,
    ...overrides,
  }
}

export function emptyUpdates(overrides: Partial<Updates> = {}): Updates {
  return {
    enabled: true,
    source: 'RealDoc06/surface-coordinator',
    last_checked_at: null,
    last_error: null,
    latest: null,
    cached: [],
    ...overrides,
  }
}

export function release(version: string, overrides: Partial<Updates['cached'][number]> = {}) {
  return {
    version,
    target: 'windows-x86_64',
    size: 4_200_000,
    sha256: 'ab'.repeat(32),
    cached_at: Date.now() - 3_600_000,
    notes: null,
    ...overrides,
  }
}

let nextId = 1

export function fakeCoordinator(initial: Partial<Omit<FakeServer, 'calls'>> = {}): FakeServer {
  const server: FakeServer = {
    session: baseSession,
    devices: [device('a', { hostname: 'LAB-01' }), device('b', { hostname: 'LAB-02' }), device('c', { hostname: 'LAB-03' })],
    groups: [],
    scripts: [],
    jobs: {},
    catalogue: [],
    updates: null,
    checkResult: null,
    calls: [],
    ...initial,
  }
  // Tests share fixtures; the server changes its own copies.
  server.devices = structuredClone(server.devices)
  server.groups = structuredClone(server.groups)
  server.scripts = structuredClone(server.scripts)
  server.jobs = structuredClone(server.jobs)

  const targetsFor = (spec: { type: string; device_ids?: string[]; group_id?: string }): string[] | Reply => {
    if (spec.type === 'all') return server.devices.map((d) => d.id)
    if (spec.type === 'devices') return spec.device_ids ?? []
    if (spec.type === 'group') {
      const found = server.groups.find((g) => g.id === spec.group_id)
      if (!found) return apiError(400, 'unknown_group', 'no such group')
      return found.device_ids
    }
    return apiError(400, 'no_targets')
  }

  server.calls = mockFetch((call) => {
    const overridden = server.override?.(call)
    if (overridden) return overridden
    const { method, path, body } = call
    if (path === '/session') return { body: server.session }
    if (path === '/devices') return { body: { devices: server.devices } }
    if (path === '/enrollment-keys') return { body: { keys: [] } }
    if (path === '/rejected-connections') return { body: { rejected: [] } }

    if (method === 'GET' && path.startsWith('/devices/')) {
      const found = server.devices.find((d) => `/devices/${d.id}` === path)
      return found ? { body: { device: found, targets: [] } } : undefined
    }

    // Groups
    if (path === '/groups' && method === 'GET') return { body: { groups: server.groups } }
    if (path === '/groups' && method === 'POST') {
      const name = String((body as { name: string }).name).trim()
      if (server.groups.some((g) => g.name.toLowerCase() === name.toLowerCase())) return apiError(409, 'name_taken')
      const created = group(`g${nextId++}`, name)
      server.groups = [...server.groups, created].sort((x, y) => x.name.localeCompare(y.name))
      return { status: 201, body: { group: created } }
    }
    const groupMembers = /^\/groups\/([^/]+)\/members$/.exec(path)
    if (groupMembers && method === 'PUT') {
      const found = server.groups.find((g) => g.id === groupMembers[1])
      if (!found) return undefined
      const ids = (body as { device_ids: string[] }).device_ids
      if (ids.some((id) => !server.devices.some((d) => d.id === id))) return apiError(400, 'unknown_device')
      found.device_ids = ids
      return { body: { group: found } }
    }
    const groupOne = /^\/groups\/([^/]+)$/.exec(path)
    if (groupOne) {
      const found = server.groups.find((g) => g.id === groupOne[1])
      if (!found) return undefined
      if (method === 'PATCH') {
        const name = String((body as { name: string }).name).trim()
        if (server.groups.some((g) => g.id !== found.id && g.name.toLowerCase() === name.toLowerCase())) {
          return apiError(409, 'name_taken')
        }
        found.name = name
        return { body: { group: found } }
      }
      if (method === 'DELETE') {
        server.groups = server.groups.filter((g) => g !== found)
        return { status: 204 }
      }
    }

    // Script library
    if (path === '/scripts' && method === 'GET') return { body: { scripts: server.scripts } }
    if (path === '/scripts' && method === 'POST') {
      const input = body as { name: string; shell: 'powershell' | 'cmd'; body: string }
      if (server.scripts.some((s) => s.name.toLowerCase() === input.name.toLowerCase())) return apiError(409, 'name_taken')
      const created = script(`s${nextId++}`, input.name, { shell: input.shell, body: input.body })
      server.scripts = [...server.scripts, created]
      return { status: 201, body: { script: created } }
    }
    const scriptOne = /^\/scripts\/([^/]+)$/.exec(path)
    if (scriptOne) {
      const found = server.scripts.find((s) => s.id === scriptOne[1])
      if (!found) return undefined
      if (found.built_in) return apiError(403, 'built_in', 'built-in scripts cannot be changed')
      if (method === 'PUT') {
        const input = body as { name: string; shell: 'powershell' | 'cmd'; body: string }
        if (server.scripts.some((s) => s.id !== found.id && s.name.toLowerCase() === input.name.toLowerCase())) {
          return apiError(409, 'name_taken')
        }
        Object.assign(found, input, { updated_at: Date.now() })
        return { body: { script: found } }
      }
      if (method === 'DELETE') {
        server.scripts = server.scripts.filter((s) => s !== found)
        return { status: 204 }
      }
    }

    if (path === '/settings-catalogue') return server.catalogue ? { body: { settings: server.catalogue } } : undefined

    // Updates
    if (path === '/updates' && method === 'GET') return server.updates ? { body: server.updates } : undefined
    if (path === '/updates/check' && method === 'POST') {
      if (server.checkResult) return server.checkResult
      return server.updates ? { body: server.updates } : undefined
    }

    // Jobs
    if (method === 'POST' && path === '/jobs') {
      const request = body as { kind: string; payload: unknown; targets: { type: string; device_ids?: string[]; group_id?: string } }
      const ids = targetsFor(request.targets)
      if (!Array.isArray(ids)) return ids
      if (ids.length === 0) return apiError(400, 'no_targets')
      const id = `job-${nextId++}`
      const created = job(id, ids.map((d, i) => target(`${id}-t${i}`, d)), { kind: request.kind, payload: request.payload })
      server.jobs[id] = created
      return { status: 201, body: { job: created } }
    }
    const cancel = /^\/jobs\/([^/]+)\/cancel$/.exec(path)
    if (cancel && method === 'POST') {
      const found = server.jobs[cancel[1] ?? '']
      if (!found) return undefined
      found.targets = found.targets.map((t) =>
        t.state === 'queued' || t.state === 'sent' || t.state === 'running'
          ? { ...t, state: 'failed', reason: 'cancelled', finished_at: Date.now() }
          : t,
      )
      return { body: { job: found } }
    }
    if (path.startsWith('/jobs?')) return { body: { jobs: Object.values(server.jobs) } }
    if (method === 'GET' && path.startsWith('/jobs/')) {
      const found = server.jobs[path.slice('/jobs/'.length)]
      return found ? { body: { job: found } } : undefined
    }
    return undefined
  })
  return server
}

export const jobPosts = (calls: Call[]) => calls.filter((c) => c.method === 'POST' && c.path === '/jobs')
export const callsTo = (calls: Call[], method: string, path: string) =>
  calls.filter((c) => c.method === method && c.path === path)
