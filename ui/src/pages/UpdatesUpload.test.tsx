import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { App } from '../App'
import type { Rollout, RolloutState, Session } from '../api/types'
import { callsTo, emptyUpdates, fakeCoordinator, release } from '../test/coordinator'
import { apiError, device, FakeWebSocket, type Call } from '../test/helpers'

beforeEach(() => {
  FakeWebSocket.install()
})

afterEach(() => {
  vi.unstubAllGlobals()
  window.history.replaceState(null, '', '/')
})

function start(path: string) {
  window.history.replaceState(null, '', path)
  const user = userEvent.setup()
  render(<App />)
  return user
}

const EXE = 'surface-coordinator-client-1.4.0-windows-x86_64.exe'

/** The requests exactly as fetch received them; the shared mock drops non-text bodies. */
const fetched = () => vi.mocked(fetch).mock.calls.map(([input, init]) => ({ url: String(input), init: init ?? {} }))

function exe(name = EXE) {
  return new File(['MZ-binary'], name, { type: 'application/octet-stream' })
}

describe('Upload a client build', () => {
  it('sends the file as the request body with the signature header, then shows the refreshed page', async () => {
    const uploaded = emptyUpdates({ latest: release('1.4.0'), cached: [release('1.4.0', { notes: 'Fixes sleep.' })] })
    const server = fakeCoordinator({ updates: emptyUpdates() })
    server.override = (call) =>
      call.method === 'POST' && call.path.startsWith('/updates/upload') ? { status: 201, body: uploaded } : undefined
    const user = start('/updates')

    const file = exe()
    await user.upload(await screen.findByLabelText('Client executable'), file)
    // The version comes from the file name.
    expect(screen.getByLabelText('Version')).toHaveValue('1.4.0')
    expect(screen.getByRole('button', { name: 'Upload' })).toBeDisabled()

    await user.type(screen.getByLabelText('Signature (base64)'), 'c2ln\nbmF0dXJl\n')
    await user.type(screen.getByLabelText('Notes (optional)'), 'Fixes sleep & more')
    await user.click(screen.getByRole('button', { name: 'Upload' }))

    expect(await screen.findByText('Uploaded 1.4.0.')).toBeInTheDocument()
    const [request] = fetched().filter((r) => r.url.startsWith('/api/v1/updates/upload'))
    expect(request).toBeDefined()
    const url = new URL(request!.url, 'http://coordinator.test')
    expect(url.pathname).toBe('/api/v1/updates/upload')
    expect(Object.fromEntries(url.searchParams)).toEqual({
      version: '1.4.0',
      target: 'windows-x86_64',
      notes: 'Fixes sleep & more',
    })
    expect(request!.init.method).toBe('POST')
    expect(request!.init.headers).toMatchObject({ 'X-Signature': 'c2lnbmF0dXJl', 'Content-Type': 'application/octet-stream' })
    expect(request!.init.body).toBe(file)

    // The refreshed data is on the page and the form is empty again.
    const table = screen.getByRole('table')
    expect(within(table).getByText('Fixes sleep.')).toBeInTheDocument()
    expect(screen.getByLabelText('Version')).toHaveValue('')
    expect(screen.getByLabelText('Signature (base64)')).toHaveValue('')
  })

  it('shows progress while uploading and does not send twice', async () => {
    const server = fakeCoordinator({ updates: emptyUpdates() })
    let finish: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      finish = resolve
    })
    server.override = (call) => (call.path.startsWith('/updates/upload') ? { status: 201, body: emptyUpdates() } : undefined)
    const user = start('/updates')
    await user.upload(await screen.findByLabelText('Client executable'), exe())
    await user.type(screen.getByLabelText('Signature (base64)'), 'c2ln')

    // Hold the upload's response back until the test lets it go.
    const real = vi.mocked(fetch).getMockImplementation()!
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      if (String(input).startsWith('/api/v1/updates/upload')) await gate
      return real(input, init)
    })

    await user.click(screen.getByRole('button', { name: 'Upload' }))
    expect(await screen.findByRole('button', { name: 'Uploading…' })).toBeDisabled()
    finish()
    expect(await screen.findByText('Uploaded 1.4.0.')).toBeInTheDocument()
    expect(fetched().filter((r) => r.url.startsWith('/api/v1/updates/upload'))).toHaveLength(1)
  })

  it('fills the signature from a .sig file', async () => {
    fakeCoordinator({ updates: emptyUpdates() })
    const user = start('/updates')
    await user.upload(await screen.findByLabelText('Signature file (.sig)'), new File(['dGhlLXNpZw==\n'], `${EXE}.sig`))
    await waitFor(() => expect(screen.getByLabelText('Signature (base64)')).toHaveValue('dGhlLXNpZw==\n'))
  })

  it('leaves the version empty for other file names and checks its shape', async () => {
    fakeCoordinator({ updates: emptyUpdates() })
    const user = start('/updates')
    await user.upload(await screen.findByLabelText('Client executable'), exe('my-build.exe'))
    const version = screen.getByLabelText('Version')
    expect(version).toHaveValue('')
    await user.type(screen.getByLabelText('Signature (base64)'), 'c2ln')
    await user.type(version, '1.4')
    expect(screen.getByText(/Three numbers separated by dots/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Upload' })).toBeDisabled()
    await user.type(version, '.0')
    expect(screen.getByRole('button', { name: 'Upload' })).toBeEnabled()
  })

  it.each([
    ['invalid_version', 400, 'The version must have three numbers, like 1.2.3.'],
    ['unsupported_target', 400, 'This coordinator does not take client builds for that platform.'],
    ['bad_signature', 400, "The signature does not match this file, version and the coordinator's update key."],
    ['version_exists', 409, 'That version has already been uploaded. Use a new version number.'],
    ['too_large', 413, 'The file is too large for this coordinator.'],
    ['updates_unavailable', 503, 'This coordinator has no update key built in, so it cannot take client builds.'],
  ])('says what %s means and keeps the form for another try', async (code, status, message) => {
    const server = fakeCoordinator({ updates: emptyUpdates() })
    server.override = (call) => (call.path.startsWith('/updates/upload') ? apiError(status, code, 'raw server text') : undefined)
    const user = start('/updates')
    await user.upload(await screen.findByLabelText('Client executable'), exe())
    await user.type(screen.getByLabelText('Signature (base64)'), 'c2ln')
    await user.click(screen.getByRole('button', { name: 'Upload' }))

    expect(await screen.findByRole('alert')).toHaveTextContent(message)
    expect(screen.getByLabelText('Version')).toHaveValue('1.4.0')
    expect(screen.getByRole('button', { name: 'Upload' })).toBeEnabled()
  })
})

describe('Automatic updates', () => {
  const autoSwitch = () => screen.findByRole('switch', { name: 'Update devices automatically' })

  it('explains the staged behaviour and posts the change', async () => {
    const server = fakeCoordinator({ updates: emptyUpdates({ auto_update: false }) })
    const user = start('/updates')
    const toggle = await autoSwitch()
    expect(toggle).not.toBeChecked()
    expect(screen.getByText(/One device is updated first; the others follow once it is back on the new version/)).toBeInTheDocument()
    expect(screen.getByText(/nothing else is updated until a device has been updated successfully by hand/)).toBeInTheDocument()

    await user.click(toggle)
    await waitFor(() => expect(toggle).toBeChecked())
    expect(callsTo(server.calls, 'POST', '/updates/auto').map((c) => c.body)).toEqual([{ enabled: true }])

    await user.click(toggle)
    await waitFor(() => expect(toggle).not.toBeChecked())
    expect(callsTo(server.calls, 'POST', '/updates/auto').map((c) => c.body)).toEqual([{ enabled: true }, { enabled: false }])
  })

  it('shows the error and keeps the switch where it was when saving fails', async () => {
    const server = fakeCoordinator({ updates: emptyUpdates({ auto_update: true }) })
    server.override = (call: Call) => (call.path === '/updates/auto' ? apiError(503, 'updates_unavailable') : undefined)
    const user = start('/updates')
    const toggle = await autoSwitch()
    await user.click(toggle)
    expect(await screen.findByRole('alert')).toHaveTextContent('no update key built in')
    expect(toggle).toBeChecked()
  })

  it('is absent from a coordinator that has no automatic updates', async () => {
    fakeCoordinator({ updates: emptyUpdates() })
    start('/updates')
    await screen.findByRole('heading', { name: 'Upload a client build' })
    expect(screen.queryByRole('switch')).not.toBeInTheDocument()
  })
})

describe('Rollout status', () => {
  const rollout = (state: RolloutState, overrides: Partial<Rollout> = {}): Rollout => ({
    version: '1.4.0',
    state,
    canary_device_id: 'b',
    updated: 3,
    pending: 5,
    failed: 1,
    ...overrides,
  })

  it.each([
    ['canary', 'Testing on one device'],
    ['rolling', 'Rolling out'],
    ['halted', 'Halted'],
    ['done', 'All devices updated'],
  ] as const)('shows the %s state, the first device and the counts', async (state, label) => {
    fakeCoordinator({ updates: emptyUpdates({ rollout: rollout(state) }) })
    start('/updates')
    const panel = (await screen.findByRole('heading', { name: 'Rollout' })).closest('section') as HTMLElement
    expect(within(panel).getByText('1.4.0')).toBeInTheDocument()
    expect(within(panel).getByText(label)).toBeInTheDocument()
    expect(within(panel).getByRole('link', { name: 'LAB-02' })).toHaveAttribute('href', '/devices/b')
    expect(within(panel).getByText('3 updated')).toBeInTheDocument()
    expect(within(panel).getByText('5 pending')).toBeInTheDocument()
    expect(within(panel).getByText('1 failed')).toBeInTheDocument()
    // Only a halted rollout needs the operator to do something.
    if (state === 'halted') expect(within(panel).getByText(/Update one device by hand from the device list/)).toBeInTheDocument()
    else expect(within(panel).queryByText(/Update one device by hand/)).not.toBeInTheDocument()
  })

  it('falls back to the device id when the device is not known, and omits it when there is none', async () => {
    fakeCoordinator({ updates: emptyUpdates({ rollout: rollout('canary', { canary_device_id: 'gone-device' }) }) })
    start('/updates')
    expect(await screen.findByRole('link', { name: 'gone-device' })).toHaveAttribute('href', '/devices/gone-device')
  })

  it('has no rollout section when there is none', async () => {
    fakeCoordinator({ updates: emptyUpdates({ rollout: null }) })
    start('/updates')
    await screen.findByRole('heading', { name: 'Upload a client build' })
    expect(screen.queryByRole('heading', { name: 'Rollout' })).not.toBeInTheDocument()
  })
})

describe('Release source', () => {
  it('is shown, with Check now, when the coordinator checks (or does not say)', async () => {
    fakeCoordinator({ updates: emptyUpdates({ checking: true }) })
    start('/updates')
    expect(await screen.findByRole('heading', { name: 'Release source' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check now' })).toBeEnabled()
  })

  it('is replaced by a line about uploads when the coordinator does not check', async () => {
    fakeCoordinator({ updates: emptyUpdates({ checking: false }) })
    start('/updates')
    expect(await screen.findByText(/Updates are provided by upload/)).toBeInTheDocument()
    expect(screen.queryByRole('heading', { name: 'Release source' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Check now' })).not.toBeInTheDocument()
  })
})

describe('Move to the manager Wi-Fi dialog', () => {
  const session: Session = {
    authenticated: true,
    auth_mode: 'token',
    account: null,
    capabilities: { accounts: false, hotspot: true },
    version: '0.6.0',
    protocol_version: 3,
  }

  async function openDialog(wifi: Record<string, unknown>) {
    const server = fakeCoordinator({ session, devices: [device('a', { hostname: 'LAB-01' })] })
    server.override = (call) =>
      call.path === '/manager/wifi'
        ? { body: { available: true, ssid: 'Office-Net', secured: true, coordinator_url: 'wss://sc.example.org', reason: null, ...wifi } }
        : undefined
    const user = start('/')
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(await screen.findByRole('button', { name: 'Move to Office-Net' }))
    return await screen.findByRole('dialog', { name: 'Move to Office-Net' })
  }

  it('says the devices go to the online coordinator and leave this one', async () => {
    const dialog = await openDialog({ coordinator: 'online' })
    expect(dialog).toHaveTextContent(
      'They will connect to the online coordinator at wss://sc.example.org from then on and leave this local one. If they cannot report back, the job shows “Destination unconfirmed” after two minutes. Check the destination coordinator before retrying.',
    )
  })

  it.each([['local', { coordinator: 'local' }], ['absent', {}]])('keeps the current wording when the coordinator is %s', async (_name, wifi) => {
    const dialog = await openDialog({ coordinator_url: 'ws://192.168.1.5:8081', ...wifi })
    expect(dialog).toHaveTextContent(
      'They will reach this coordinator at ws://192.168.1.5:8081 from then on. A device that cannot join comes back to the hotspot and reports the failure.',
    )
    expect(dialog).not.toHaveTextContent('online coordinator')
  })
})
