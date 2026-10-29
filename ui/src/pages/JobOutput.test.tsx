import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { App } from '../App'
import { api } from '../api/client'
import { callsTo, fakeCoordinator } from '../test/coordinator'
import { FakeWebSocket, job, target } from '../test/helpers'

type Snapshot = Awaited<ReturnType<typeof api.getJobOutput>>

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}

function coordinator() {
  return fakeCoordinator({ jobs: { j1: job('j1', [
    target('t1', 'a', { state: 'running', output: 'first' }),
    target('t2', 'b', { state: 'running', output: 'second' }),
  ]) } })
}

beforeEach(() => {
  FakeWebSocket.install()
  window.history.replaceState(null, '', '/jobs/j1')
})

afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  window.history.replaceState(null, '', '/')
})

it('loads metadata and one selected snapshot, merging crossing events by Unicode offsets', async () => {
  const server = coordinator()
  const pending = deferred<Snapshot>()
  const reads = vi.spyOn(api, 'getJobOutput').mockImplementationOnce(() => pending.promise)
  render(<App />)
  const pane = await screen.findByLabelText('Output of LAB-01')
  expect(pane).toHaveTextContent('Loading output')
  expect(reads).toHaveBeenCalledExactlyOnceWith('j1', 't1')
  expect(callsTo(server.calls, 'GET', '/jobs/j1?include_output=false')).toHaveLength(1)
  expect(callsTo(server.calls, 'GET', '/jobs/j1')).toHaveLength(0)
  vi.useFakeTimers()
  act(() => {
    FakeWebSocket.latest().send({ type: 'job.output', job_id: 'j1', job_target_id: 't1', offset: 0, data: '😀\0', truncated: false })
    FakeWebSocket.latest().send({ type: 'job.output', job_id: 'j1', job_target_id: 't1', offset: 2, data: 'tail', truncated: true })
    FakeWebSocket.latest().send({ type: 'job.output', job_id: 'j1', job_target_id: 't2', offset: 0, data: 'not selected', truncated: false })
  })
  await act(() => vi.advanceTimersByTimeAsync(40))
  await act(async () => pending.resolve({ output: '😀\0', output_truncated: false }))
  expect(pane.textContent).toBe('😀\0tail')
  expect(screen.getByRole('note')).toHaveTextContent('Output was truncated')
  expect(reads).toHaveBeenCalledTimes(1)
})

it('ignores stale responses after switching away and back to the same target', async () => {
  coordinator()
  const first = deferred<Snapshot>()
  const second = deferred<Snapshot>()
  const latest = deferred<Snapshot>()
  const reads = vi.spyOn(api, 'getJobOutput')
    .mockImplementationOnce(() => first.promise)
    .mockImplementationOnce(() => second.promise)
    .mockImplementationOnce(() => latest.promise)
  const user = userEvent.setup()
  render(<App />)
  await screen.findByLabelText('Output of LAB-01')
  await user.click(screen.getByRole('button', { name: 'LAB-02' }))
  await user.click(screen.getByRole('button', { name: 'LAB-01' }))
  expect(reads.mock.calls).toEqual([['j1', 't1'], ['j1', 't2'], ['j1', 't1']])
  await act(async () => latest.resolve({ output: 'current first', output_truncated: false }))
  await act(async () => second.resolve({ output: 'wrong device', output_truncated: true }))
  await act(async () => first.resolve({ output: 'stale first', output_truncated: true }))
  expect(screen.getByLabelText('Output of LAB-01').textContent).toBe('current first')
  expect(screen.queryByRole('note')).not.toBeInTheDocument()
})

it('refreshes only the selected output on completion and recovers missed output on reconnect', async () => {
  const server = coordinator()
  const reads = vi.spyOn(api, 'getJobOutput')
  render(<App />)
  const pane = await screen.findByLabelText('Output of LAB-01')
  await waitFor(() => expect(pane.textContent).toBe('first'))
  const metadataBefore = callsTo(server.calls, 'GET', '/jobs/j1?include_output=false').length

  act(() => FakeWebSocket.latest().send({ type: 'job.target.updated', job_id: 'j1', target: target('t2', 'b', { state: 'succeeded' }) }))
  expect(reads).toHaveBeenCalledTimes(1)
  server.jobs.j1!.targets[0] = target('t1', 'a', { state: 'succeeded', output: 'first complete' })
  act(() => FakeWebSocket.latest().send({ type: 'job.target.updated', job_id: 'j1', target: server.jobs.j1!.targets[0] }))
  await waitFor(() => expect(pane.textContent).toBe('first complete'))
  expect(reads).toHaveBeenCalledTimes(2)
  expect(callsTo(server.calls, 'GET', '/jobs/j1?include_output=false')).toHaveLength(metadataBefore)

  const pending = deferred<Snapshot>()
  reads.mockImplementationOnce(() => pending.promise)
  act(() => FakeWebSocket.latest().open())
  await waitFor(() => expect(reads).toHaveBeenCalledTimes(3))
  expect(pane.textContent).toBe('first complete')
  vi.useFakeTimers()
  act(() => FakeWebSocket.latest().send({ type: 'job.output', job_id: 'j1', job_target_id: 't1', offset: 21, data: ' tail', truncated: false }))
  await act(() => vi.advanceTimersByTimeAsync(40))
  await act(async () => pending.resolve({ output: 'first complete missed', output_truncated: false }))
  expect(pane.textContent).toBe('first complete missed tail')
  expect(callsTo(server.calls, 'GET', '/jobs/j1?include_output=false')).toHaveLength(metadataBefore + 1)
  expect(reads.mock.calls.every(([, targetId]) => targetId === 't1')).toBe(true)
})

it('offers a scoped retry when the output request fails', async () => {
  const server = coordinator()
  const reads = vi.spyOn(api, 'getJobOutput').mockRejectedValueOnce(new Error('Output read failed'))
  const user = userEvent.setup()
  render(<App />)
  expect(await screen.findByText('Output read failed')).toBeInTheDocument()
  expect(screen.getByLabelText('Output of LAB-01')).toHaveTextContent('Output could not be loaded')
  await user.click(screen.getByRole('button', { name: 'Try again' }))
  await waitFor(() => expect(screen.getByLabelText('Output of LAB-01').textContent).toBe('first'))
  expect(reads).toHaveBeenCalledTimes(2)
  expect(callsTo(server.calls, 'GET', '/jobs/j1?include_output=false')).toHaveLength(1)
})
