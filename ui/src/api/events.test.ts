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
})
