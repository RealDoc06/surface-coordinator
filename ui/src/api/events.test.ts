import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { FakeWebSocket } from '../test/helpers'
import { backoffDelay, eventsUrl, EventStream, type ConnectionStatus } from './events'

describe('eventsUrl', () => {
  it('uses ws on http and wss on https, on the same host', () => {
    expect(eventsUrl({ protocol: 'http:', host: '192.168.137.1:8080' })).toBe('ws://192.168.137.1:8080/api/v1/events')
    expect(eventsUrl({ protocol: 'https:', host: 'sc.example.org' })).toBe('wss://sc.example.org/api/v1/events')
  })
})

describe('backoffDelay', () => {
  it('grows exponentially with jitter and is capped', () => {
    expect(backoffDelay(0, () => 1)).toBe(1000)
    expect(backoffDelay(0, () => 0)).toBe(500)
    expect(backoffDelay(3, () => 1)).toBe(8000)
    expect(backoffDelay(50, () => 1)).toBe(30_000)
  })
})

describe('EventStream', () => {
  let events: unknown[]
  let statuses: ConnectionStatus[]
  let stream: EventStream

  beforeEach(() => {
    vi.useFakeTimers()
    FakeWebSocket.install()
    events = []
    statuses = []
    stream = new EventStream({
      url: 'ws://test/api/v1/events',
      onEvent: (e) => events.push(e),
      onStatus: (s) => statuses.push(s),
    })
  })

  afterEach(() => {
    stream.stop()
    vi.useRealTimers()
    vi.unstubAllGlobals()
  })

  it('delivers parsed messages and skips ones that are not JSON', () => {
    stream.start()
    const socket = FakeWebSocket.latest()
    expect(socket.url).toBe('ws://test/api/v1/events')
    socket.open()
    socket.send({ type: 'resync' })
    socket.send('not json')
    socket.send({ type: 'something.new', x: 1 })
    expect(events).toEqual([{ type: 'resync' }, { type: 'something.new', x: 1 }])
    expect(statuses).toEqual(['connecting', 'open'])
  })

  it('reconnects with growing delays after the socket drops', () => {
    vi.spyOn(Math, 'random').mockReturnValue(1)
    stream.start()
    FakeWebSocket.latest().open()
    FakeWebSocket.latest().drop()
    expect(statuses).toEqual(['connecting', 'open', 'reconnecting'])
    expect(FakeWebSocket.instances).toHaveLength(1)

    vi.advanceTimersByTime(999)
    expect(FakeWebSocket.instances).toHaveLength(1)
    vi.advanceTimersByTime(1)
    expect(FakeWebSocket.instances).toHaveLength(2)

    // Second failure in a row waits twice as long.
    FakeWebSocket.latest().drop()
    vi.advanceTimersByTime(1999)
    expect(FakeWebSocket.instances).toHaveLength(2)
    vi.advanceTimersByTime(1)
    expect(FakeWebSocket.instances).toHaveLength(3)

    // A successful connection resets the delay.
    FakeWebSocket.latest().open()
    expect(statuses[statuses.length - 1]).toBe('open')
    FakeWebSocket.latest().drop()
    vi.advanceTimersByTime(1000)
    expect(FakeWebSocket.instances).toHaveLength(4)
  })

  it('stops for good when stopped', () => {
    stream.start()
    const socket = FakeWebSocket.latest()
    socket.open()
    stream.stop()
    expect(socket.closedByClient).toBe(true)
    vi.advanceTimersByTime(120_000)
    expect(FakeWebSocket.instances).toHaveLength(1)
  })

  it('retries a handshake that never completes', () => {
    vi.spyOn(Math, 'random').mockReturnValue(1)
    stream.start()
    const stalled = FakeWebSocket.latest()
    vi.advanceTimersByTime(10_000)
    expect(stalled.closedByClient).toBe(true)
    expect(statuses.at(-1)).toBe('reconnecting')
    vi.advanceTimersByTime(1000)
    expect(FakeWebSocket.instances).toHaveLength(2)
    FakeWebSocket.latest().open()
    expect(statuses.at(-1)).toBe('open')
  })

  it('reconnects immediately when the network returns and removes listeners on stop', () => {
    stream.start()
    const oldSocket = FakeWebSocket.latest()
    oldSocket.open()
    window.dispatchEvent(new Event('online'))
    expect(oldSocket.closedByClient).toBe(true)
    expect(FakeWebSocket.instances).toHaveLength(2)
    stream.stop()
    window.dispatchEvent(new Event('online'))
    vi.advanceTimersByTime(60_000)
    expect(FakeWebSocket.instances).toHaveLength(2)
  })

  it('lets an operator bypass backoff without leaving duplicate retries', () => {
    stream.start()
    FakeWebSocket.latest().drop()
    stream.reconnect()
    expect(FakeWebSocket.instances).toHaveLength(2)
    FakeWebSocket.latest().open()
    vi.advanceTimersByTime(30_000)
    expect(FakeWebSocket.instances).toHaveLength(2)
  })

  it('recovers an open socket that silently stops receiving messages', () => {
    vi.spyOn(Math, 'random').mockReturnValue(1)
    stream.start()
    const socket = FakeWebSocket.latest()
    socket.open()
    vi.advanceTimersByTime(34_999)
    expect(statuses.at(-1)).toBe('open')
    vi.advanceTimersByTime(1)
    expect(socket.closedByClient).toBe(true)
    expect(statuses.at(-1)).toBe('reconnecting')
    vi.advanceTimersByTime(1000)
    expect(FakeWebSocket.instances).toHaveLength(2)
    FakeWebSocket.latest().open()
    expect(statuses.at(-1)).toBe('open')
  })

  it('uses heartbeats and events as liveness without rendering heartbeats', () => {
    stream.start()
    const socket = FakeWebSocket.latest()
    socket.open()
    for (let i = 0; i < 6; i++) {
      vi.advanceTimersByTime(10_000)
      socket.send({ type: 'heartbeat' })
    }
    expect(events).toEqual([])
    expect(statuses).toEqual(['connecting', 'open'])
    expect(FakeWebSocket.instances).toHaveLength(1)

    vi.advanceTimersByTime(34_000)
    socket.send({ type: 'device.removed', device_id: 'gone' })
    vi.advanceTimersByTime(34_000)
    expect(statuses.at(-1)).toBe('open')
    expect(events).toEqual([{ type: 'device.removed', device_id: 'gone' }])
    stream.stop()
    expect(vi.getTimerCount()).toBe(0)
    vi.advanceTimersByTime(60_000)
    expect(FakeWebSocket.instances).toHaveLength(1)
  })

  it('checks stale activity when returning from sleep before delayed timers fire', () => {
    vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('visible')
    stream.start()
    const socket = FakeWebSocket.latest()
    socket.open()
    document.dispatchEvent(new Event('visibilitychange'))
    expect(FakeWebSocket.instances).toHaveLength(1)
    // Changing wall time simulates a suspended tab without running its timers.
    vi.setSystemTime(Date.now() + 35_000)
    document.dispatchEvent(new Event('visibilitychange'))
    expect(socket.closedByClient).toBe(true)
    expect(FakeWebSocket.instances).toHaveLength(2)
    expect(statuses.at(-1)).toBe('reconnecting')
  })

  it('does not let malformed messages keep a dead event stream live', () => {
    stream.start()
    const socket = FakeWebSocket.latest()
    socket.open()
    vi.advanceTimersByTime(30_000)
    socket.send('not JSON')
    vi.advanceTimersByTime(5000)
    expect(socket.closedByClient).toBe(true)
    expect(statuses.at(-1)).toBe('reconnecting')
  })
})
