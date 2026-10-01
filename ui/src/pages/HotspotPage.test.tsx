import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { App } from '../App'
import type { Hotspot, Key, Session } from '../api/types'
import { apiError, FakeWebSocket, mockFetch, type Call } from '../test/helpers'

const session = (hotspot: boolean): Session => ({
  authenticated: true,
  auth_mode: 'token',
  account: null,
  capabilities: { accounts: false, hotspot },
  version: '0.3.0',
  protocol_version: 2,
})

function key(id: string, label: string, overrides: Partial<Key> = {}): Key {
  return { id, label, created_at: 1_000, revoked_at: null, device_count: 0, ...overrides }
}

const KEY_ID = 'abcdef0123456789abcdef0123456789'

function hotspot(overrides: Partial<Hotspot> = {}): Hotspot {
  return {
    control: 'automatic',
    state: 'off',
    key_id: null,
    ssid: null,
    passphrase: null,
    client_port: 8081,
    error: null,
    instructions: [],
    ...overrides,
  }
}

const running = (overrides: Partial<Hotspot> = {}) =>
  hotspot({ state: 'on', key_id: KEY_ID, ssid: 'SC-abcdef01', passphrase: 'pass-phrase-123', ...overrides })

type Options = {
  hotspot?: Hotspot
  keys?: Key[]
  hotspotAvailable?: boolean
  /** Decides what start does; defaults to starting successfully. */
  onStart?: (keyId: string) => { status?: number; body?: unknown }
}

/** A small manager behind the mocked fetch: the hotspot is whatever the last start/stop left. */
function manager(options: Options = {}) {
  const server = {
    hotspot: options.hotspot ?? hotspot(),
    keys: options.keys ?? [key(KEY_ID, 'Demo room')],
    calls: [] as Call[],
  }
  server.calls = mockFetch((call) => {
    const { method, path } = call
    if (path === '/session') return { body: session(options.hotspotAvailable ?? true) }
    if (path === '/devices') return { body: { devices: [] } }
    if (path === '/enrollment-keys') return { body: { keys: server.keys } }
    if (path === '/rejected-connections') return { body: { rejected: [] } }
    if (method === 'GET' && path === '/manager/hotspot') return { body: server.hotspot }
    if (method === 'POST' && path === '/manager/hotspot/start') {
      const keyId = (call.body as { key_id: string }).key_id
      if (options.onStart) {
        const reply = options.onStart(keyId)
        if (reply.status !== undefined && reply.status >= 400) return reply
        server.hotspot = reply.body as Hotspot
        return reply
      }
      server.hotspot = running({ key_id: keyId, ssid: `SC-${keyId.slice(0, 8)}` })
      return { body: server.hotspot }
    }
    if (method === 'POST' && path === '/manager/hotspot/stop') {
      server.hotspot = hotspot({ control: server.hotspot.control })
      return { body: server.hotspot }
    }
    return undefined
  })
  return server
}

const posts = (calls: Call[], path: string) => calls.filter((c) => c.method === 'POST' && c.path === path)
const gets = (calls: Call[]) => calls.filter((c) => c.method === 'GET' && c.path === '/manager/hotspot')

beforeEach(() => {
  FakeWebSocket.install()
  window.history.replaceState(null, '', '/hotspot')
})

afterEach(() => {
  vi.unstubAllGlobals()
  window.history.replaceState(null, '', '/')
})

describe('Hotspot navigation', () => {
  it('has a Hotspot entry only when the manager offers hotspot control', async () => {
    manager()
    render(<App />)
    expect(await screen.findByRole('link', { name: 'Hotspot' })).toHaveAttribute('href', '/hotspot')
  })

  it('has none otherwise, and the page says it is not available', async () => {
    const server = manager({ hotspotAvailable: false })
    render(<App />)
    expect(await screen.findByText('Hotspot control is not available here')).toBeInTheDocument()
    expect(screen.queryByRole('link', { name: 'Hotspot' })).not.toBeInTheDocument()
    expect(gets(server.calls)).toHaveLength(0)
  })
})

describe('Hotspot, off', () => {
  it('explains what happens and points to Move to Wi-Fi', async () => {
    manager()
    render(<App />)
    expect(await screen.findByText(/Devices enrolled with the chosen enrollment key look for the hotspot/)).toBeInTheDocument()
    expect(screen.getByText(/on port 8081/)).toBeInTheDocument()
    expect(screen.getByText(/Move to Wi-Fi/)).toBeInTheDocument()
    expect(screen.getByText('Hotspot is off')).toBeInTheDocument()
  })

  it('offers only keys that are not revoked', async () => {
    manager({
      keys: [key('key-active-0001', 'Room A'), key('key-gone-0002', 'Old room', { revoked_at: 5_000 }), key('key-active-0003', 'Room B')],
    })
    render(<App />)
    const select = await screen.findByRole('combobox', { name: 'Enrollment key' })
    expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual([
      'Room A (key-acti…)',
      'Room B (key-acti…)',
    ])
  })

  it('says so, and links to the keys page, when there is no usable key', async () => {
    manager({ keys: [key('k-gone-1', 'Old room', { revoked_at: 5_000 })] })
    render(<App />)
    expect(await screen.findByText(/There is no active enrollment key/)).toBeInTheDocument()
    expect(screen.getAllByRole('link', { name: 'Enrollment keys' }).map((a) => a.getAttribute('href'))).toContain('/keys')
    expect(screen.queryByRole('button', { name: 'Start hotspot' })).not.toBeInTheDocument()
  })

  it('starts the hotspot for the chosen key and then shows its network and passphrase', async () => {
    const server = manager({ keys: [key('k-aaaaaaaa-1', 'Room A'), key('k-bbbbbbbb-2', 'Room B')] })
    const user = userEvent.setup()
    render(<App />)

    await user.selectOptions(await screen.findByRole('combobox', { name: 'Enrollment key' }), 'k-bbbbbbbb-2')
    await user.click(screen.getByRole('button', { name: 'Start hotspot' }))

    expect(await screen.findByText('Hotspot is on')).toBeInTheDocument()
    expect(posts(server.calls, '/manager/hotspot/start').map((c) => c.body)).toEqual([{ key_id: 'k-bbbbbbbb-2' }])
    expect(screen.getByDisplayValue('SC-k-bbbbbb')).toBeInTheDocument()
    expect(screen.getByLabelText('Passphrase')).toHaveValue('pass-phrase-123')
    expect(screen.getByText('8081')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Stop hotspot' })).toBeInTheDocument()
    // The state is read back from the manager after the action.
    expect(gets(server.calls).length).toBeGreaterThanOrEqual(2)
  })

  it('shows it as busy while the start request is in flight', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((resolve) => (release = resolve))
    const server = manager()
    const original = globalThis.fetch
    vi.stubGlobal('fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
      if (String(input).endsWith('/manager/hotspot/start')) await gate
      return original(input, init)
    })
    const user = userEvent.setup()
    render(<App />)
    await user.click(await screen.findByRole('button', { name: 'Start hotspot' }))

    expect(await screen.findByText(/Starting the hotspot/)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Start hotspot' })).not.toBeInTheDocument()
    release()
    expect(await screen.findByText('Hotspot is on')).toBeInTheDocument()
    expect(posts(server.calls, '/manager/hotspot/start')).toHaveLength(1)
  })
})

describe('Hotspot, errors', () => {
  async function startWith(reply: ReturnType<typeof apiError>) {
    const server = manager({ onStart: () => reply })
    const user = userEvent.setup()
    render(<App />)
    await user.click(await screen.findByRole('button', { name: 'Start hotspot' }))
    return { server, user }
  }

  it('shows hotspot_failed with the reason the manager gives', async () => {
    await startWith(apiError(502, 'hotspot_failed', 'The Wi-Fi adapter does not support hosted networks.'))
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'The hotspot could not be started: The Wi-Fi adapter does not support hosted networks.',
    )
    // Still possible to try again.
    expect(screen.getByRole('button', { name: 'Start hotspot' })).toBeInTheDocument()
  })

  it('shows key_revoked inline', async () => {
    await startWith(apiError(400, 'key_revoked', 'revoked'))
    expect(await screen.findByRole('alert')).toHaveTextContent(/enrollment key has been revoked/)
  })

  it('shows not_found inline', async () => {
    await startWith(apiError(404, 'not_found', 'no such key'))
    expect(await screen.findByRole('alert')).toHaveTextContent(/does not exist any more/)
  })

  it('shows an error state reported by the manager, with its message and a retry', async () => {
    const server = manager({
      hotspot: hotspot({ state: 'error', key_id: KEY_ID, error: 'Could not enable the mobile hotspot.' }),
    })
    const user = userEvent.setup()
    render(<App />)

    expect(await screen.findByText('The hotspot did not start')).toBeInTheDocument()
    expect(screen.getByRole('alert')).toHaveTextContent('Could not enable the mobile hotspot.')
    await user.click(screen.getByRole('button', { name: 'Try again' }))
    expect(await screen.findByText('Hotspot is on')).toBeInTheDocument()
    expect(posts(server.calls, '/manager/hotspot/start').map((c) => c.body)).toEqual([{ key_id: KEY_ID }])
  })

  it('shows a failure to read the hotspot, with a retry', async () => {
    let fail = true
    mockFetch((call) => {
      if (call.path === '/session') return { body: session(true) }
      if (call.path === '/devices') return { body: { devices: [] } }
      if (call.path === '/enrollment-keys') return { body: { keys: [key(KEY_ID, 'Demo room')] } }
      if (call.path === '/rejected-connections') return { body: { rejected: [] } }
      if (call.path === '/manager/hotspot') return fail ? apiError(500, 'internal', 'manager exploded') : { body: hotspot() }
      return undefined
    })
    const user = userEvent.setup()
    render(<App />)
    expect(await screen.findByRole('alert')).toHaveTextContent('manager exploded')
    fail = false
    await user.click(screen.getByRole('button', { name: 'Try again' }))
    expect(await screen.findByText('Hotspot is off')).toBeInTheDocument()
  })
})

describe('Hotspot, on', () => {
  it('shows the network, passphrase, port and key, and stops the hotspot', async () => {
    const server = manager({ hotspot: running() })
    const user = userEvent.setup()
    render(<App />)

    expect(await screen.findByText('Hotspot is on')).toBeInTheDocument()
    expect(screen.getByLabelText('Network name (SSID)')).toHaveValue('SC-abcdef01')
    expect(screen.getByLabelText('Passphrase')).toHaveValue('pass-phrase-123')
    expect(screen.getByText('8081')).toBeInTheDocument()
    expect(screen.getByText('Demo room')).toBeInTheDocument()
    expect(screen.getAllByRole('button', { name: /Copy/ })).toHaveLength(2)

    await user.click(screen.getByRole('button', { name: 'Stop hotspot' }))
    expect(await screen.findByText('Hotspot is off')).toBeInTheDocument()
    expect(posts(server.calls, '/manager/hotspot/stop')).toHaveLength(1)
  })

  it('keeps asking while the hotspot is starting, and shows it once it is on', async () => {
    const server = manager({ hotspot: hotspot({ state: 'starting', key_id: KEY_ID }) })
    render(<App />)
    expect(await screen.findByText(/Starting the hotspot/)).toBeInTheDocument()

    server.hotspot = running()
    expect(await screen.findByText('Hotspot is on', undefined, { timeout: 5000 })).toBeInTheDocument()
    expect(gets(server.calls).length).toBeGreaterThanOrEqual(2)
  })
})

describe('Hotspot, manual control', () => {
  const steps = ['Open System Settings, then General, then Sharing.', 'Turn on Internet Sharing.', 'Share over Wi-Fi.']

  it('says the operator turns it on, with numbered steps, network and passphrase', async () => {
    manager({ hotspot: running({ control: 'manual', instructions: steps }) })
    render(<App />)

    expect(await screen.findByText('Turn the hotspot on yourself')).toBeInTheDocument()
    expect(screen.getByText(/The manager cannot switch the hotspot on for this computer: you do it/)).toBeInTheDocument()
    const list = screen.getByRole('list', { name: 'Steps' })
    expect(list.tagName).toBe('OL')
    expect(within(list).getAllByRole('listitem').map((li) => li.textContent)).toEqual(steps)
    expect(screen.getByLabelText('Network name (SSID)')).toHaveValue('SC-abcdef01')
    expect(screen.getByLabelText('Passphrase')).toHaveValue('pass-phrase-123')
    expect(screen.getByText(/Stopping here does not turn the hotspot off/)).toBeInTheDocument()
  })

  it('asks for the key first and does not promise to start anything', async () => {
    const server = manager({
      hotspot: hotspot({ control: 'manual' }),
      onStart: (keyId) => ({ body: running({ control: 'manual', key_id: keyId, instructions: ['Turn it on.'] }) }),
    })
    const user = userEvent.setup()
    render(<App />)

    expect(await screen.findByText(/you turn the hotspot on yourself/)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Start hotspot' })).not.toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Show hotspot settings' }))
    expect(await screen.findByText('Turn the hotspot on yourself')).toBeInTheDocument()
    expect(posts(server.calls, '/manager/hotspot/start')).toHaveLength(1)
  })
})

describe('Hotspot, unsupported', () => {
  it('explains that this computer cannot do it and offers nothing to start', async () => {
    manager({ hotspot: hotspot({ control: 'unsupported' }) })
    render(<App />)
    expect(await screen.findByText('This computer cannot run a hotspot for you')).toBeInTheDocument()
    expect(screen.getByText(/another way to give the devices a network to meet on/)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /hotspot/i })).not.toBeInTheDocument()
    await waitFor(() => expect(screen.queryByRole('combobox')).not.toBeInTheDocument())
  })
})
