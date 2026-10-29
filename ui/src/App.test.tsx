import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { App } from './App'
import type { Device, Job, Session } from './api/types'
import { apiError, device, FakeWebSocket, job, jobRead, mockFetch, target, type Call } from './test/helpers'

const base: Session = {
  authenticated: true,
  auth_mode: 'accounts',
  account: { id: 'acc-1', username: 'admin' },
  capabilities: { accounts: true, hotspot: false },
  version: '0.2.0',
  protocol_version: 1,
}

/** A small in-memory coordinator behind the mocked fetch. */
function coordinator(initial: Partial<Session> = {}) {
  const server = {
    session: { ...base, ...initial } as Session,
    devices: [device('a', { hostname: 'LAB-01' })] as Device[],
    jobs: {} as Record<string, Job>,
    token: 'secret-token',
    calls: [] as Call[],
  }
  server.calls = mockFetch((call) => {
    const { method, path } = call
    if (path === '/session') return { body: server.session }
    if (method === 'POST' && path === '/auth/token') {
      if ((call.body as { token: string }).token !== server.token) return apiError(401, 'invalid_token')
      server.session = { ...server.session, authenticated: true }
      return { status: 204 }
    }
    if (method === 'POST' && path === '/auth/login') {
      const { username, password } = call.body as { username: string; password: string }
      if (username !== 'admin' || password !== 'correct horse') return apiError(401, 'invalid_credentials')
      server.session = { ...server.session, authenticated: true, account: { id: 'acc-1', username } }
      return { body: { account: { id: 'acc-1', username, created_at: 1 } } }
    }
    if (method === 'POST' && path === '/auth/logout') {
      server.session = { ...server.session, authenticated: false, account: null }
      return { status: 204 }
    }
    if (!server.session.authenticated) return apiError(401, 'unauthorized')
    if (path === '/devices') return { body: { devices: server.devices } }
    if (path === '/enrollment-keys') return { body: { keys: [] } }
    if (path === '/rejected-connections') return { body: { rejected: [] } }
    if (path.startsWith('/jobs?')) return { body: { jobs: Object.values(server.jobs) } }
    if (method === 'GET' && path.startsWith('/jobs/')) {
      return jobRead(path, server.jobs)
    }
    return undefined
  })
  return server
}

const count = (calls: Call[], path: string) => calls.filter((c) => c.path === path).length

beforeEach(() => {
  FakeWebSocket.install()
  window.history.replaceState(null, '', '/')
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('startup', () => {
  it('exchanges the token from the URL fragment and removes it', async () => {
    const server = coordinator({ authenticated: false, auth_mode: 'token', account: null })
    window.history.replaceState(null, '', '/#token=secret-token')
    render(<App />)

    expect(await screen.findByRole('link', { name: 'LAB-01' })).toBeInTheDocument()
    expect(server.calls.slice(0, 2).map((c) => [c.method, c.path, c.body])).toEqual([
      ['POST', '/auth/token', { token: 'secret-token' }],
      ['GET', '/session', undefined],
    ])
    expect(window.location.hash).toBe('')
    expect(window.location.href).not.toContain('secret-token')
    // No accounts in token mode: no Accounts page, nobody to log out.
    expect(screen.queryByRole('button', { name: 'Log out' })).not.toBeInTheDocument()
  })

  it('tells the user to open the UI from the manager in token mode without a token', async () => {
    const server = coordinator({ authenticated: false, auth_mode: 'token', account: null })
    render(<App />)
    expect(await screen.findByText(/Open this page from the manager/)).toBeInTheDocument()
    expect(count(server.calls, '/auth/token')).toBe(0)
    expect(FakeWebSocket.instances).toHaveLength(0)
  })

  it('reports a token that is not accepted', async () => {
    coordinator({ authenticated: false, auth_mode: 'token', account: null })
    window.history.replaceState(null, '', '/#token=stale')
    render(<App />)
    expect(await screen.findByRole('alert')).toHaveTextContent(/token was not accepted/)
    expect(window.location.hash).toBe('')
  })

  it('shows the login form in accounts mode and signs in', async () => {
    coordinator({ authenticated: false, account: null })
    const user = userEvent.setup()
    render(<App />)

    await user.type(await screen.findByLabelText('Username'), 'admin')
    await user.type(screen.getByLabelText('Password'), 'wrong')
    await user.click(screen.getByRole('button', { name: 'Sign in' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('Wrong username or password.')

    await user.type(screen.getByLabelText('Password'), 'correct horse')
    await user.click(screen.getByRole('button', { name: 'Sign in' }))
    expect(await screen.findByRole('link', { name: 'LAB-01' })).toBeInTheDocument()
    expect(screen.getByText('admin')).toBeInTheDocument()

    await user.click(screen.getByRole('button', { name: 'Log out' }))
    expect(await screen.findByLabelText('Username')).toBeInTheDocument()
  })

  it('explains throttled logins', async () => {
    const server = coordinator({ authenticated: false, account: null })
    server.calls.length = 0
    mockFetch((call) =>
      call.path === '/session' ? { body: server.session } : apiError(429, 'too_many_attempts'),
    )
    const user = userEvent.setup()
    render(<App />)
    await user.type(await screen.findByLabelText('Username'), 'admin')
    await user.type(screen.getByLabelText('Password'), 'whatever')
    await user.click(screen.getByRole('button', { name: 'Sign in' }))
    expect(await screen.findByRole('alert')).toHaveTextContent(/Too many failed attempts/)
  })
})

describe('live updates', () => {
  async function start() {
    const server = coordinator()
    const user = userEvent.setup()
    render(<App />)
    await screen.findByRole('link', { name: 'LAB-01' })
    const socket = FakeWebSocket.latest()
    act(() => socket.open())
    return { server, user, socket }
  }

  it('opens one events socket on the page origin', async () => {
    const { socket } = await start()
    expect(FakeWebSocket.instances).toHaveLength(1)
    expect(socket.url).toBe(`ws://${window.location.host}/api/v1/events`)
  })

  it('updates the device list from events without reloading', async () => {
    const { server, socket } = await start()
    await waitFor(() => expect(count(server.calls, '/devices')).toBe(2))
    const before = count(server.calls, '/devices')

    act(() => socket.send({ type: 'device.updated', device: device('b', { hostname: 'LAB-02' }) }))
    expect(screen.getByRole('link', { name: 'LAB-02' })).toBeInTheDocument()

    act(() => socket.send({ type: 'device.updated', device: device('a', { hostname: 'LAB-01', online: false }) }))
    expect(within(screen.getByRole('row', { name: /LAB-01/ })).getByRole('img', { name: 'Offline' })).toBeInTheDocument()

    act(() => socket.send({ type: 'device.removed', device_id: 'b' }))
    expect(screen.queryByRole('link', { name: 'LAB-02' })).not.toBeInTheDocument()

    act(() => socket.send({ type: 'from.the.future', anything: true }))
    expect(screen.getByRole('link', { name: 'LAB-01' })).toBeInTheDocument()
    expect(count(server.calls, '/devices')).toBe(before)
  })

  it('refetches on resync', async () => {
    const { server, socket } = await start()
    await waitFor(() => expect(count(server.calls, '/devices')).toBe(2))

    server.devices = [device('a', { hostname: 'LAB-01' }), device('c', { hostname: 'LAB-03' })]
    act(() => socket.send({ type: 'resync' }))

    expect(await screen.findByRole('link', { name: 'LAB-03' })).toBeInTheDocument()
    expect(count(server.calls, '/devices')).toBe(3)
    expect(count(server.calls, '/rejected-connections')).toBe(3)
  })

  it('shows that live updates are paused and refetches after reconnecting', async () => {
    const { server, socket } = await start()
    await waitFor(() => expect(count(server.calls, '/devices')).toBe(2))
    expect(screen.queryByRole('status')).not.toBeInTheDocument()

    vi.useFakeTimers()
    try {
      act(() => socket.drop())
      expect(screen.getByRole('status')).toHaveTextContent('Live updates paused, reconnecting')

      server.devices = [device('z', { hostname: 'NEW-01' })]
      act(() => vi.advanceTimersByTime(1000))
      expect(FakeWebSocket.instances).toHaveLength(2)
      act(() => FakeWebSocket.latest().open())
      expect(screen.queryByRole('status')).not.toBeInTheDocument()
    } finally {
      vi.useRealTimers()
    }
    expect(await screen.findByRole('link', { name: 'NEW-01' })).toBeInTheDocument()
  })

  it('badges rejected connections that arrive while the user is elsewhere', async () => {
    const { user, socket } = await start()
    const nav = screen.getByRole('navigation', { name: 'Main' })
    const rejected = { id: 7, at: 1000, peer: '10.0.0.5:4000', device_id: null, hostname: 'ROGUE', key_id: null, reason: 'unknown enrollment key' }

    act(() => socket.send({ type: 'connection.rejected', rejected }))
    expect(within(nav).getByLabelText('1 new')).toBeInTheDocument()

    // Rejected connections is in the Administration menu, which carries the badge.
    await user.click(within(nav).getByRole('button', { name: /Administration/ }))
    await user.click(within(nav).getByRole('menuitem', { name: /Rejected connections/ }))
    expect(await screen.findByText('unknown enrollment key')).toBeInTheDocument()
    expect(screen.getByText('ROGUE')).toBeInTheDocument()
    await waitFor(() => expect(within(nav).queryByLabelText('1 new')).not.toBeInTheDocument())

    act(() => socket.send({ type: 'connection.rejected', rejected: { ...rejected, id: 8, hostname: 'ROGUE-2' } }))
    expect(screen.getByText('ROGUE-2')).toBeInTheDocument()
    await waitFor(() => expect(within(nav).queryByLabelText(/new$/)).not.toBeInTheDocument())
  })

  it('shows job states and output live on the job page', async () => {
    const { server, user, socket } = await start()
    server.jobs.j1 = job('j1', [target('t1', 'a', { state: 'running', started_at: 6000, output: 'first\n' })])

    await user.click(screen.getByRole('link', { name: 'Jobs' }))
    await user.click(await screen.findByRole('link', { name: 'Get-Date' }))

    const pane = await screen.findByLabelText('Output of LAB-01')
    await waitFor(() => expect(pane).toHaveTextContent('first'))

    act(() => {
      socket.send({ type: 'job.output', job_id: 'j1', job_target_id: 't1', data: 'second\n', truncated: false })
      socket.send({ type: 'job.output', job_id: 'j1', job_target_id: 't1', data: 'third\n[truncated]', truncated: true })
    })
    await waitFor(() => expect(pane.textContent).toBe('first\nsecond\nthird\n[truncated]'))
    expect(screen.getByRole('note')).toHaveTextContent(/Output was truncated/)

    // The coordinator stores the same; finishing makes the page read it back.
    server.jobs.j1 = job('j1', [
      target('t1', 'a', {
        state: 'failed',
        exit_code: 3,
        reason: 'nonzero_exit',
        started_at: 6000,
        finished_at: 8500,
        output: 'first\nsecond\nthird\n[truncated]',
        output_truncated: true,
      }),
    ])
    const loadsBefore = count(server.calls, '/jobs/j1/targets/t1/output')
    const metadataBefore = count(server.calls, '/jobs/j1?include_output=false')
    act(() =>
      socket.send({
        type: 'job.target.updated',
        job_id: 'j1',
        target: target('t1', 'a', { state: 'failed', exit_code: 3, reason: 'nonzero_exit', started_at: 6000, finished_at: 8500, output_truncated: true }),
      }),
    )
    const row = screen.getByRole('row', { name: /LAB-01/ })
    expect(within(row).getByText('failed')).toBeInTheDocument()
    expect(within(row).getByText('3')).toBeInTheDocument()
    expect(within(row).getByText('2.5 s')).toBeInTheDocument()
    expect(within(row).getByText('Non-zero exit code')).toBeInTheDocument()
    await waitFor(() => expect(count(server.calls, '/jobs/j1/targets/t1/output')).toBe(loadsBefore + 1))
    expect(count(server.calls, '/jobs/j1?include_output=false')).toBe(metadataBefore)
    expect(pane.textContent).toBe('first\nsecond\nthird\n[truncated]')
  })

  it('places live output by offset, ignoring what the loaded output already holds', async () => {
    const { server, user, socket } = await start()
    // Two code points of emoji: 4 UTF-16 code units, but offset 2.
    server.jobs.j1 = job('j1', [target('t1', 'a', { state: 'running', started_at: 6000, output: '\u{1F600}\u{1F600}' })])

    await user.click(screen.getByRole('link', { name: 'Jobs' }))
    await user.click(await screen.findByRole('link', { name: 'Get-Date' }))
    const pane = await screen.findByLabelText('Output of LAB-01')
    await waitFor(() => expect(pane.textContent).toBe('\u{1F600}\u{1F600}'))

    const event = { type: 'job.output', job_id: 'j1', job_target_id: 't1', truncated: false }
    act(() => {
      socket.send({ ...event, offset: 1, data: '\u{1F600}' }) // already shown
      socket.send({ ...event, offset: 2, data: 'a' })
      socket.send({ ...event, offset: 2, data: 'a' }) // delivered twice
      socket.send({ ...event, offset: 3, data: 'b' })
    })
    await waitFor(() => expect(pane.textContent).toBe('\u{1F600}\u{1F600}ab'))
  })

  it('shows a job as gone when its last device is forgotten while it is open', async () => {
    const { server, user, socket } = await start()
    server.jobs.j1 = job('j1', [target('t1', 'a', { state: 'succeeded', exit_code: 0, started_at: 6000, finished_at: 7000 })])

    await user.click(screen.getByRole('link', { name: 'Jobs' }))
    await user.click(await screen.findByRole('link', { name: 'Get-Date' }))
    await screen.findByLabelText('Output of LAB-01')

    act(() => socket.send({ type: 'device.removed', device_id: 'a' }))
    expect(await screen.findByText('Job not found')).toBeInTheDocument()
  })

  it('drops a job from the list when its last device is forgotten', async () => {
    const { server, user, socket } = await start()
    server.jobs.j1 = job('j1', [target('t1', 'a')])
    await user.click(screen.getByRole('link', { name: 'Jobs' }))
    await screen.findByRole('link', { name: 'Get-Date' })

    act(() => socket.send({ type: 'device.removed', device_id: 'a' }))
    expect(screen.queryByRole('link', { name: 'Get-Date' })).not.toBeInTheDocument()
    expect(screen.getByText('No jobs yet')).toBeInTheDocument()
  })

  it('returns to the login form when the session has expired', async () => {
    const { server, socket } = await start()
    server.session = { ...server.session, authenticated: false, account: null }
    act(() => socket.send({ type: 'resync' }))
    expect(await screen.findByLabelText('Username')).toBeInTheDocument()
  })
})
