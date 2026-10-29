import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { App } from '../App'
import { api } from '../api/client'
import type { ManagerWifi, Updates } from '../api/types'
import { callsTo, emptyUpdates, fakeCoordinator } from '../test/coordinator'
import { apiError, device, FakeWebSocket, job, target } from '../test/helpers'

beforeEach(() => {
  FakeWebSocket.install()
  window.history.replaceState(null, '', '/updates')
})

afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  window.history.replaceState(null, '', '/')
})

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}

it('coalesces rollout events, ignores heartbeats, and never overlaps background reads', async () => {
  const initial = emptyUpdates({ auto_update: true, rollout: {
    version: '0.2.3', state: 'canary', canary_device_id: 'a', updated: 0, pending: 1, failed: 0,
  } })
  const server = fakeCoordinator({ devices: [device('a')], updates: initial })
  const reads = vi.spyOn(api, 'getUpdates')
  const view = render(<App />)
  await screen.findByText('Testing on one device')
  const baseline = reads.mock.calls.length
  const pending = deferred<Updates>()
  reads.mockImplementationOnce(() => pending.promise)
  vi.useFakeTimers()

  act(() => {
    for (let i = 0; i < 100; i++) {
      FakeWebSocket.latest().send({ type: 'job.target.updated', job_id: 'old-update', target: target(`t${i}`, 'a') })
    }
  })
  expect(reads).toHaveBeenCalledTimes(baseline)
  await act(() => vi.advanceTimersByTimeAsync(250))
  expect(reads).toHaveBeenCalledTimes(baseline + 1)

  act(() => {
    for (let i = 0; i < 100; i++) {
      FakeWebSocket.latest().send({ type: 'job.target.updated', job_id: 'old-update', target: target(`t${i}`, 'a', { state: 'succeeded' }) })
    }
  })
  await act(() => vi.advanceTimersByTimeAsync(1000))
  expect(reads).toHaveBeenCalledTimes(baseline + 1)
  server.updates = emptyUpdates({ ...initial, rollout: {
    version: '0.2.3', state: 'done', canary_device_id: 'a', updated: 1, pending: 0, failed: 0,
  } })
  await act(async () => pending.resolve(initial))
  await act(() => vi.advanceTimersByTimeAsync(250))
  expect(screen.getByText('All devices updated')).toBeInTheDocument()
  expect(reads).toHaveBeenCalledTimes(baseline + 2)

  act(() => {
    FakeWebSocket.latest().send({ type: 'heartbeat' })
    FakeWebSocket.latest().send({ type: 'device.updated', device: device('a', { last_seen_at: 50_000 }) })
    FakeWebSocket.latest().send({ type: 'job.output', job_id: 'noisy', job_target_id: 't1', data: 'output', truncated: false })
  })
  await act(() => vi.advanceTimersByTimeAsync(500))
  expect(reads).toHaveBeenCalledTimes(baseline + 2)

  act(() => FakeWebSocket.latest().send({ type: 'device.updated', device: device('a', { client_version: '0.2.3' }) }))
  view.unmount()
  await act(() => vi.advanceTimersByTimeAsync(500))
  expect(reads).toHaveBeenCalledTimes(baseline + 2)
})

it('keeps an accepted setting change when an older rollout read completes afterward', async () => {
  const initial = emptyUpdates({ auto_update: true })
  fakeCoordinator({ updates: initial })
  const reads = vi.spyOn(api, 'getUpdates')
  const user = userEvent.setup()
  render(<App />)
  const toggle = await screen.findByRole('switch', { name: 'Update devices automatically' })
  const baseline = reads.mock.calls.length
  const pending = deferred<Updates>()
  reads.mockImplementationOnce(() => pending.promise)
  act(() => FakeWebSocket.latest().send({ type: 'job.target.updated', job_id: 'old-update', target: target('t1', 'a') }))
  await waitFor(() => expect(reads).toHaveBeenCalledTimes(baseline + 1))
  await user.click(toggle)
  await waitFor(() => expect(toggle).not.toBeChecked())
  await act(async () => pending.resolve(initial))
  expect(toggle).not.toBeChecked()
})

it('keeps the reviewed Wi-Fi destination stable and requires confirmation again after a conflict', async () => {
  window.history.replaceState(null, '', '/')
  const server = fakeCoordinator({ devices: [device('a')], session: {
    authenticated: true, auth_mode: 'token', account: null,
    capabilities: { accounts: false, hotspot: true }, version: '0.2.3', protocol_version: 3,
  } })
  let wifi: ManagerWifi = {
    available: true, ssid: 'Reviewed Wi-Fi', secured: true, reason: null,
    coordinator: 'online', coordinator_url: 'wss://reviewed.example.org',
  }
  let moves = 0
  server.override = (call) => {
    if (call.path === '/manager/wifi') return { body: wifi }
    if (call.path === '/manager/wifi/move') {
      moves += 1
      if (moves === 1) return apiError(409, 'wifi_changed', 'Review the changed destination.')
      const created = job('wifi-move', [target('t1', 'a')], { kind: 'wifi.join', payload: { ssid: wifi.ssid } })
      server.jobs[created.id] = created
      return { status: 201, body: { job: created } }
    }
    return undefined
  }
  const user = userEvent.setup()
  render(<App />)
  await user.click(await screen.findByRole('checkbox', { name: 'Select SURFACE-A' }))
  await user.click(await screen.findByRole('button', { name: 'Move to Reviewed Wi-Fi' }))
  const reviewed = await screen.findByRole('dialog', { name: 'Move to Reviewed Wi-Fi' })

  wifi = { ...wifi, ssid: 'New Wi-Fi', coordinator_url: 'wss://new.example.org' }
  act(() => FakeWebSocket.latest().send({ type: 'resync' }))
  await screen.findByRole('button', { name: 'Move to New Wi-Fi' })
  expect(reviewed).toHaveTextContent('wss://reviewed.example.org')
  await user.click(within(reviewed).getByRole('button', { name: 'Move devices' }))
  const refreshed = await screen.findByRole('dialog', { name: 'Move to New Wi-Fi' })
  expect(refreshed).toHaveTextContent('wss://new.example.org')
  expect(within(refreshed).getByRole('alert')).toHaveTextContent('confirm again')
  expect(moves).toBe(1)
  await user.click(within(refreshed).getByRole('button', { name: 'Move devices' }))
  await screen.findByRole('heading', { name: 'Wi-Fi job' })
  expect(callsTo(server.calls, 'POST', '/manager/wifi/move').map((call) => call.body)).toEqual([
    { targets: { type: 'devices', device_ids: ['a'] }, expected_ssid: 'Reviewed Wi-Fi', expected_coordinator_url: 'wss://reviewed.example.org' },
    { targets: { type: 'devices', device_ids: ['a'] }, expected_ssid: 'New Wi-Fi', expected_coordinator_url: 'wss://new.example.org' },
  ])
})
