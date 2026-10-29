// Test doubles for the two things the UI talks to: fetch and the events
// WebSocket. There is no live coordinator in tests.

import { vi } from 'vitest'
import type { Device, Job, Target } from '../api/types'

export type Call = { method: string; path: string; body: unknown }

type Reply = { status?: number; body?: unknown }
type Handler = (call: Call) => Reply | undefined | Promise<Reply | undefined>

/**
 * Replaces global fetch. `handler` answers a request, or returns undefined
 * for a 404. Returns the list of calls made, in order.
 */
export function mockFetch(handler: Handler): Call[] {
  const calls: Call[] = []
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), 'http://coordinator.test')
      const call: Call = {
        method: init?.method ?? 'GET',
        path: url.pathname.replace(/^\/api\/v1/, '') + url.search,
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      }
      calls.push(call)
      if (!String(input).startsWith('/api/v1/')) throw new Error(`not a relative API URL: ${String(input)}`)
      if (init?.credentials !== 'same-origin') throw new Error('credentials must be same-origin')
      const reply = (await handler(call)) ?? {
        status: 404,
        body: { error: { code: 'not_found', message: 'not found' } },
      }
      const status = reply.status ?? 200
      return new Response(status === 204 ? null : JSON.stringify(reply.body ?? {}), {
        status,
        headers: { 'Content-Type': 'application/json' },
      })
    }),
  )
  return calls
}

export function apiError(status: number, code: string, message = code): Reply {
  return { status, body: { error: { code, message } } }
}

/** Job metadata and the independently loaded output of one target. */
export function jobRead(path: string, jobs: Record<string, Job>): Reply | undefined {
  const url = new URL(path, 'http://coordinator.test')
  const route = /^\/jobs\/([^/]+)(?:\/targets\/([^/]+)\/output)?$/.exec(url.pathname)
  if (!route?.[1]) return undefined
  const found = jobs[decodeURIComponent(route[1])]
  if (!found) return undefined
  if (route[2]) {
    const target = found.targets.find((t) => t.id === decodeURIComponent(route[2]!))
    return target ? { body: { output: target.output ?? '', output_truncated: target.output_truncated } } : undefined
  }
  const targets = url.searchParams.get('include_output') === 'false'
    ? found.targets.map(({ output: _output, ...target }) => target)
    : found.targets
  return { body: { job: { ...found, targets } } }
}

export class FakeWebSocket {
  static instances: FakeWebSocket[] = []

  readonly url: string
  onopen: (() => void) | null = null
  onmessage: ((event: { data: unknown }) => void) | null = null
  onclose: (() => void) | null = null
  onerror: (() => void) | null = null
  closedByClient = false

  constructor(url: string) {
    this.url = url
    FakeWebSocket.instances.push(this)
  }

  close(): void {
    this.closedByClient = true
  }

  // Driven by the test, playing the server.
  open(): void {
    this.onopen?.()
  }

  send(event: unknown): void {
    this.onmessage?.({ data: typeof event === 'string' ? event : JSON.stringify(event) })
  }

  drop(): void {
    this.onclose?.()
  }

  static install(): void {
    FakeWebSocket.instances = []
    vi.stubGlobal('WebSocket', FakeWebSocket)
  }

  static latest(): FakeWebSocket {
    const socket = FakeWebSocket.instances[FakeWebSocket.instances.length - 1]
    if (!socket) throw new Error('no WebSocket was opened')
    return socket
  }
}

export function device(id: string, overrides: Partial<Device> = {}): Device {
  return {
    id,
    hostname: `SURFACE-${id.toUpperCase()}`,
    os: 'Windows 11 Pro',
    client_version: '0.2.0',
    enrollment_key_id: 'key-1',
    online: true,
    first_seen_at: 1_000,
    last_seen_at: 2_000,
    ...overrides,
  }
}

export function target(id: string, deviceId: string, overrides: Partial<Target> = {}): Target {
  return {
    id,
    device_id: deviceId,
    state: 'queued',
    exit_code: null,
    reason: null,
    message: null,
    output_truncated: false,
    created_at: 5_000,
    started_at: null,
    finished_at: null,
    ...overrides,
  }
}

export function job(id: string, targets: Target[], overrides: Partial<Job> = {}): Job {
  return {
    id,
    kind: 'script.run',
    payload: { shell: 'powershell', body: 'Get-Date', timeout_secs: 300, run_as: 'system' },
    created_by: 'admin',
    created_at: 5_000,
    targets,
    ...overrides,
  }
}
