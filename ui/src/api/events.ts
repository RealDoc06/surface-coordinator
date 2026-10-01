// The live-events WebSocket: one per app, reconnecting with backoff. It only
// delivers parsed messages; what they mean is decided in state/store.ts.

import { API_BASE } from './client'

export type ConnectionStatus = 'connecting' | 'open' | 'reconnecting'

export function eventsUrl(loc: Pick<Location, 'protocol' | 'host'> = window.location): string {
  const scheme = loc.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${scheme}//${loc.host}${API_BASE}/events`
}

const BASE_DELAY_MS = 1000
const MAX_DELAY_MS = 30_000

/** Delay before reconnect attempt number `attempt` (0-based), with jitter. */
export function backoffDelay(attempt: number, random: () => number = Math.random): number {
  const ceiling = Math.min(MAX_DELAY_MS, BASE_DELAY_MS * 2 ** Math.min(attempt, 16))
  return Math.round(ceiling * (0.5 + random() * 0.5))
}

export type EventStreamOptions = {
  onEvent: (event: unknown) => void
  onStatus: (status: ConnectionStatus) => void
  url?: string
}

export class EventStream {
  private readonly opts: EventStreamOptions
  private socket: WebSocket | null = null
  private timer: ReturnType<typeof setTimeout> | null = null
  private attempt = 0
  private stopped = true

  constructor(opts: EventStreamOptions) {
    this.opts = opts
  }

  start(): void {
    if (!this.stopped) return
    this.stopped = false
    this.attempt = 0
    this.opts.onStatus('connecting')
    this.connect()
  }

  stop(): void {
    this.stopped = true
    if (this.timer !== null) clearTimeout(this.timer)
    this.timer = null
    const socket = this.socket
    this.socket = null
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null
      socket.close()
    }
  }

  private connect(): void {
    let socket: WebSocket
    try {
      socket = new WebSocket(this.opts.url ?? eventsUrl())
    } catch {
      this.scheduleReconnect()
      return
    }
    this.socket = socket
    socket.onopen = () => {
      this.attempt = 0
      this.opts.onStatus('open')
    }
    socket.onmessage = (msg: MessageEvent) => {
      if (typeof msg.data !== 'string') return
      let parsed: unknown
      try {
        parsed = JSON.parse(msg.data)
      } catch {
        return
      }
      this.opts.onEvent(parsed)
    }
    socket.onclose = () => {
      if (this.socket !== socket) return
      this.socket = null
      this.scheduleReconnect()
    }
    // An error is always followed by close; reconnecting is handled there.
    socket.onerror = () => {}
  }

  private scheduleReconnect(): void {
    if (this.stopped) return
    this.opts.onStatus('reconnecting')
    const delay = backoffDelay(this.attempt)
    this.attempt += 1
    this.timer = setTimeout(() => {
      this.timer = null
      if (!this.stopped) this.connect()
    }, delay)
  }
}
