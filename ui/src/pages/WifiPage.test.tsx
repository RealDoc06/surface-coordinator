import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { App } from '../App'
import type { Device, Job, Session } from '../api/types'
import { device, FakeWebSocket, job, jobRead, mockFetch, target, type Call } from '../test/helpers'

const session: Session = {
  authenticated: true,
  auth_mode: 'token',
  account: null,
  capabilities: { accounts: false, hotspot: false },
  version: '0.3.0',
  protocol_version: 2,
}

const devices: Device[] = [
  device('a', { hostname: 'LAB-01', network: { ssid: 'SC-abcdef01', ip: '192.168.137.20' } }),
  device('b', { hostname: 'LAB-02', network: { ssid: null, ip: null } }),
  device('c', { hostname: 'LAB-03' }),
]

function coordinator(extraJobs: Job[] = []) {
  const jobs: Record<string, Job> = Object.fromEntries(extraJobs.map((j) => [j.id, j]))
  const calls: Call[] = mockFetch((call) => {
    const { method, path } = call
    if (path === '/session') return { body: session }
    if (path === '/devices') return { body: { devices } }
    if (path === '/enrollment-keys') return { body: { keys: [] } }
    if (path === '/rejected-connections') return { body: { rejected: [] } }
    if (method === 'GET' && path.startsWith('/devices/')) {
      const found = devices.find((d) => `/devices/${d.id}` === path)
      return found ? { body: { device: found, targets: [] } } : undefined
    }
    if (method === 'POST' && path === '/jobs') {
      const request = call.body as { kind: string; payload: Record<string, unknown>; targets: { type: string; device_ids?: string[] } }
      const ids = request.targets.type === 'all' ? devices.map((d) => d.id) : (request.targets.device_ids ?? [])
      // The coordinator stores the password masked.
      const payload = 'password' in request.payload ? { ...request.payload, password: '********' } : request.payload
      const created = job('new-job', ids.map((id, i) => target(`nt${i}`, id)), { kind: request.kind, payload })
      jobs[created.id] = created
      return { status: 201, body: { job: created } }
    }
    if (path.startsWith('/jobs?')) return { body: { jobs: Object.values(jobs) } }
    if (method === 'GET' && path.startsWith('/jobs/')) {
      return jobRead(path, jobs)
    }
    return undefined
  })
  return calls
}

const jobPosts = (calls: Call[]) => calls.filter((c) => c.method === 'POST' && c.path === '/jobs')

beforeEach(() => {
  FakeWebSocket.install()
  window.history.replaceState(null, '', '/')
})

afterEach(() => {
  vi.unstubAllGlobals()
  window.history.replaceState(null, '', '/')
})

describe('Move to Wi-Fi', () => {
  it('moves the selected devices: devices page, form, job page', async () => {
    const calls = coordinator()
    const user = userEvent.setup()
    render(<App />)

    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('checkbox', { name: 'Select LAB-03' }))
    await user.click(screen.getByRole('button', { name: /^Move to Wi-Fi/ }))

    expect(await screen.findByRole('heading', { name: 'Move to Wi-Fi' })).toBeInTheDocument()
    expect(screen.getByText('2 selected devices: LAB-01, LAB-03')).toBeInTheDocument()
    await user.type(screen.getByRole('textbox', { name: 'Network name (SSID)' }), 'Classroom')
    await user.type(screen.getByLabelText('Password'), 'correct horse')
    await user.click(screen.getByRole('button', { name: /^Move 2 devices/ }))

    expect(await screen.findByRole('heading', { name: 'Wi-Fi job' })).toBeInTheDocument()
    expect(jobPosts(calls).map((c) => c.body)).toEqual([
      {
        kind: 'wifi.join',
        payload: { ssid: 'Classroom', password: 'correct horse' },
        targets: { type: 'devices', device_ids: ['a', 'c'] },
      },
    ])
    // The payload view shows the password as the coordinator returns it.
    expect(screen.getByText('Classroom')).toBeInTheDocument()
    expect(screen.getByText('********')).toBeInTheDocument()
    expect(screen.queryByText('correct horse')).not.toBeInTheDocument()
  })

  it('moves all devices', async () => {
    const calls = coordinator()
    const user = userEvent.setup()
    render(<App />)

    await user.click(await screen.findByRole('button', { name: 'All devices' }))
    await user.click(screen.getByRole('menuitem', { name: /^Move all to Wi-Fi/ }))
    expect(await screen.findByText('All devices (3)')).toBeInTheDocument()
    await user.type(screen.getByRole('textbox', { name: 'Network name (SSID)' }), 'Open net')
    await user.click(screen.getByRole('button', { name: /^Move 3 devices/ }))

    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toEqual({
      kind: 'wifi.join',
      payload: { ssid: 'Open net' },
      targets: { type: 'all' },
    })
  })

  it('moves one device from its detail page', async () => {
    coordinator()
    const user = userEvent.setup()
    window.history.replaceState(null, '', '/devices/c')
    render(<App />)

    await user.click(await screen.findByRole('button', { name: /^Move to Wi-Fi/ }))
    expect(await screen.findByText('1 selected device: LAB-03')).toBeInTheDocument()
  })

  it('runs a Wi-Fi job again with the form filled in except for the password', async () => {
    const previous = job('old-wifi', [target('t1', 'a'), target('t2', 'b')], {
      kind: 'wifi.join',
      payload: { ssid: 'Classroom', password: '********', coordinator_url: 'ws://10.0.0.1:8081' },
    })
    coordinator([previous])
    const user = userEvent.setup()
    window.history.replaceState(null, '', '/jobs/old-wifi')
    render(<App />)

    expect(await screen.findByText('ws://10.0.0.1:8081')).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Run again' }))

    expect(await screen.findByRole('heading', { name: 'Move to Wi-Fi' })).toBeInTheDocument()
    expect(screen.getByRole('textbox', { name: 'Network name (SSID)' })).toHaveValue('Classroom')
    expect(screen.getByRole('textbox', { name: /Coordinator address/ })).toHaveValue('ws://10.0.0.1:8081')
    expect(screen.getByLabelText('Password')).toHaveValue('')
    expect(screen.getByText(/enter it again/)).toBeInTheDocument()
    expect(screen.getByText('2 selected devices: LAB-01, LAB-02')).toBeInTheDocument()
  })

  it('lists Wi-Fi jobs by their one-line preview', async () => {
    const previous = job('old-wifi', [target('t1', 'a')], { kind: 'wifi.join', payload: { ssid: 'Classroom' } })
    coordinator([previous])
    const user = userEvent.setup()
    render(<App />)
    await user.click(await screen.findByRole('link', { name: 'Jobs' }))
    const row = (await screen.findByRole('link', { name: 'Join Wi-Fi Classroom' })).closest('tr')
    expect(row).not.toBeNull()
    expect(within(row as HTMLElement).getByText('Wi-Fi')).toBeInTheDocument()
  })
})

describe('Device network', () => {
  it('shows the SSID and address in the list', async () => {
    coordinator()
    render(<App />)
    const row = (await screen.findByRole('link', { name: 'LAB-01' })).closest('tr') as HTMLElement
    expect(within(row).getByText('SC-abcdef01')).toBeInTheDocument()
    expect(within(row).getByText('192.168.137.20')).toBeInTheDocument()
  })

  it('shows a dash for a device that reported nothing, and for an older coordinator', async () => {
    coordinator()
    render(<App />)
    for (const name of ['LAB-02', 'LAB-03']) {
      const row = (await screen.findByRole('link', { name })).closest('tr') as HTMLElement
      expect(within(row).getAllByRole('cell')[4]).toHaveTextContent('—')
    }
  })

  it('shows it on the device page, with a dash when unknown', async () => {
    coordinator()
    window.history.replaceState(null, '', '/devices/a')
    render(<App />)
    const field = (await screen.findByText('Network', { selector: 'dt' })).parentElement as HTMLElement
    expect(field).toHaveTextContent('SC-abcdef01')
    expect(field).toHaveTextContent('192.168.137.20')
  })

  it('shows a dash on the device page of a device without network information', async () => {
    coordinator()
    window.history.replaceState(null, '', '/devices/c')
    render(<App />)
    const field = (await screen.findByText('Network', { selector: 'dt' })).parentElement as HTMLElement
    expect(field).toHaveTextContent('—')
  })
})
