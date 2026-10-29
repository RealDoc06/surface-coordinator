import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { api, READ_TIMEOUT_MS, WRITE_TIMEOUT_MS } from './client'

describe('request deadlines', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => {
    vi.useRealTimers()
    vi.unstubAllGlobals()
  })

  function stallRequest() {
    const fetch = vi.fn((_url: RequestInfo | URL, init?: RequestInit) => new Promise<Response>((_resolve, reject) => {
      init?.signal?.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')))
    }))
    vi.stubGlobal('fetch', fetch)
    return fetch
  }

  it('bounds a stalled read and releases its timer', async () => {
    stallRequest()
    const result = api.listDevices().catch((err: unknown) => err)
    await vi.advanceTimersByTimeAsync(READ_TIMEOUT_MS)
    expect(await result).toMatchObject({ code: 'request_timeout', message: expect.stringContaining('Try again') })
    expect(vi.getTimerCount()).toBe(0)
  })

  it('does not retry a timed-out write and explains that it may have completed', async () => {
    const fetch = stallRequest()
    const result = api.forgetDevice('test-device').catch((err: unknown) => err)
    await vi.advanceTimersByTimeAsync(WRITE_TIMEOUT_MS)
    expect(await result).toMatchObject({ code: 'request_timeout', message: expect.stringContaining('may have completed') })
    expect(fetch).toHaveBeenCalledTimes(1)
    expect(vi.getTimerCount()).toBe(0)
  })

  it('reads current state without a browser cache and clears the deadline on success', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ devices: [] })))
    vi.stubGlobal('fetch', fetch)
    await expect(api.listDevices()).resolves.toEqual({ devices: [] })
    expect(fetch).toHaveBeenCalledWith('/api/v1/devices', expect.objectContaining({ cache: 'no-store', credentials: 'same-origin' }))
    expect(vi.getTimerCount()).toBe(0)
  })
})
