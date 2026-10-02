import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { App } from '../App'
import { callsTo, emptyUpdates, fakeCoordinator, group, jobPosts, release } from '../test/coordinator'
import { apiError, device, FakeWebSocket, job, target } from '../test/helpers'

beforeEach(() => {
  FakeWebSocket.install()
  window.history.replaceState(null, '', '/')
})

afterEach(() => {
  vi.unstubAllGlobals()
  window.history.replaceState(null, '', '/')
})

function start(path = '/') {
  window.history.replaceState(null, '', path)
  const user = userEvent.setup()
  render(<App />)
  return user
}

const devices = [
  device('a', { hostname: 'LAB-01', client_version: '1.2.9', protocol_version: 3, update_available: true }),
  device('b', { hostname: 'LAB-02', client_version: '1.2.10', protocol_version: 3, update_available: false }),
  device('c', { hostname: 'OLD-01', client_version: '0.1.0', protocol_version: 1, restricted: true, update_available: true }),
]

describe('client versions in the UI', () => {
  it('marks outdated and restricted devices in the list and the nav', async () => {
    fakeCoordinator({ devices })
    start()
    const row = (await screen.findByRole('link', { name: 'LAB-01' })).closest('tr') as HTMLElement
    expect(within(row).getByText('1.2.9')).toBeInTheDocument()
    expect(within(row).getByText('update available')).toBeInTheDocument()
    const fine = screen.getByRole('link', { name: 'LAB-02' }).closest('tr') as HTMLElement
    expect(within(fine).queryByText('update available')).not.toBeInTheDocument()
    const old = screen.getByRole('link', { name: 'OLD-01' }).closest('tr') as HTMLElement
    const restricted = within(old).getByText('restricted')
    expect(restricted).toHaveAttribute('title', expect.stringContaining('only accepts client updates'))
    // Two devices have an update available.
    expect(within(screen.getByRole('navigation', { name: 'Main' })).getByLabelText('2 outdated')).toBeInTheDocument()
  })

  it('shows the same on the device page, with the protocol version', async () => {
    fakeCoordinator({ devices })
    start('/devices/c')
    const field = (await screen.findByText('Client version', { selector: 'dt' })).parentElement as HTMLElement
    expect(field).toHaveTextContent('0.1.0')
    expect(field).toHaveTextContent('update available')
    expect(field).toHaveTextContent('restricted')
    expect(screen.getByText('Protocol version', { selector: 'dt' }).parentElement).toHaveTextContent('1')
  })

  it('shows plain versions for a coordinator that sends none of the new fields', async () => {
    fakeCoordinator({ devices: [device('a', { hostname: 'LAB-01' })] })
    start('/devices/a')
    const field = (await screen.findByText('Client version', { selector: 'dt' })).parentElement as HTMLElement
    expect(field).toHaveTextContent('0.2.0')
    expect(field).not.toHaveTextContent('update available')
    expect(screen.queryByText('Protocol version')).not.toBeInTheDocument()
  })

  it('keeps up with device.updated events that clear the marker', async () => {
    fakeCoordinator({ devices })
    start()
    await screen.findByRole('link', { name: 'LAB-01' })
    act(() => FakeWebSocket.latest().open())
    act(() =>
      FakeWebSocket.latest().send({
        type: 'device.updated',
        device: { ...devices[0]!, client_version: '1.2.10', update_available: false },
      }),
    )
    const row = screen.getByRole('link', { name: 'LAB-01' }).closest('tr') as HTMLElement
    expect(within(row).queryByText('update available')).not.toBeInTheDocument()
    expect(within(row).getByText('1.2.10')).toBeInTheDocument()
  })
})

describe('Updates page', () => {
  it('hides the upload form and says why when the build has no update key', async () => {
    fakeCoordinator({ updates: emptyUpdates({ enabled: false, source: null }) })
    start('/updates')
    expect(await screen.findByText(/this coordinator has no update key built in/)).toBeInTheDocument()
    expect(screen.queryByLabelText('Client executable')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check now' })).toBeDisabled()
  })

  it('shows the source, when it was last checked, the newest version and the cached ones', async () => {
    const latest = release('1.2.10', { size: 4_200_000, notes: 'Fixes the sleep timeout.\nSecond line.' })
    fakeCoordinator({
      devices,
      updates: emptyUpdates({
        last_checked_at: Date.now() - 5 * 60_000,
        latest,
        cached: [latest, release('1.2.3', { notes: null })],
      }),
    })
    start('/updates')

    expect(await screen.findByText('RealDoc06/surface-coordinator')).toBeInTheDocument()
    expect(screen.getByText('5 min ago')).toBeInTheDocument()
    const newest = screen.getByRole('heading', { name: 'Newest client version' }).closest('section') as HTMLElement
    expect(within(newest).getByText('1.2.10')).toBeInTheDocument()
    expect(within(newest).getByText(/4\.0 MB/)).toBeInTheDocument()
    expect(within(newest).getByText(/Fixes the sleep timeout\./)).toBeInTheDocument()
    expect(within(newest).getByText('2 devices')).toBeInTheDocument()

    const table = screen.getByRole('table')
    expect(within(table).getAllByRole('row').slice(1).map((r) => within(r).getAllByRole('cell')[0]?.textContent)).toEqual(['1.2.10', '1.2.3'])
  })

  it('shows plain text for notes, never markup', async () => {
    const latest = release('1.2.10', { notes: '<img src=x onerror=alert(1)> **bold**' })
    fakeCoordinator({ updates: emptyUpdates({ latest, cached: [latest] }) })
    start('/updates')
    const notes = (await screen.findAllByText(/onerror=alert/))[0] as HTMLElement
    expect(notes.textContent).toBe('<img src=x onerror=alert(1)> **bold**')
    expect(notes.querySelector('img')).toBeNull()
  })

  it('says when nothing has been checked or downloaded', async () => {
    fakeCoordinator({ updates: emptyUpdates() })
    start('/updates')
    expect(await screen.findByText('never')).toBeInTheDocument()
    expect(screen.getByText(/No client version has been downloaded yet/)).toBeInTheDocument()
    expect(screen.getByText('None.')).toBeInTheDocument()
  })

  it('shows the last error', async () => {
    fakeCoordinator({ updates: emptyUpdates({ last_error: 'GitHub answered 403', last_checked_at: Date.now() }) })
    start('/updates')
    expect(await screen.findByText(/Last check failed: GitHub answered 403/)).toBeInTheDocument()
  })

  it('checks now and shows the new result', async () => {
    const server = fakeCoordinator({ updates: emptyUpdates() })
    const user = start('/updates')
    await screen.findByText('never')
    const latest = release('2.0.0')
    server.checkResult = { body: emptyUpdates({ last_checked_at: Date.now(), latest, cached: [latest] }) }
    await user.click(screen.getByRole('button', { name: 'Check now' }))
    expect(await screen.findByText('just now')).toBeInTheDocument()
    expect(screen.getAllByText('2.0.0').length).toBeGreaterThan(0)
    expect(callsTo(server.calls, 'POST', '/updates/check')).toHaveLength(1)
  })

  it('shows why a check failed (502) without losing the page', async () => {
    const server = fakeCoordinator({ updates: emptyUpdates({ latest: release('1.2.3'), cached: [release('1.2.3')] }) })
    const user = start('/updates')
    await screen.findByRole('heading', { name: 'Newest client version' })
    server.checkResult = apiError(502, 'update_check_failed', 'could not reach github.com: timed out')
    server.updates = emptyUpdates({ latest: release('1.2.3'), cached: [release('1.2.3')], last_error: 'could not reach github.com: timed out' })
    await user.click(screen.getByRole('button', { name: 'Check now' }))
    const alerts = await screen.findAllByRole('alert')
    expect(alerts.map((a) => a.textContent).join(' ')).toContain('could not reach github.com: timed out')
    expect(screen.getByText('1.2.3', { selector: 'strong' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check now' })).toBeEnabled()
  })

  it('counts devices with an update available and updates exactly those', async () => {
    const latest = release('1.2.10')
    const server = fakeCoordinator({ devices, updates: emptyUpdates({ latest, cached: [latest, release('1.2.3')] }) })
    const user = start('/updates')
    await user.click(await screen.findByRole('button', { name: /Update all outdated devices/ }))

    expect(await screen.findByRole('heading', { name: 'Update client' })).toBeInTheDocument()
    expect(screen.getByText('2 selected devices: LAB-01, OLD-01')).toBeInTheDocument()
    expect(await screen.findByRole('combobox', { name: 'Install version' })).toHaveValue('1.2.10')
    await user.click(screen.getByRole('button', { name: 'Update 2 devices to 1.2.10…' }))
    await user.click(within(screen.getByRole('dialog', { name: 'Update 2 devices to 1.2.10?' })).getByRole('button', { name: 'Update' }))

    expect(await screen.findByRole('heading', { name: 'Client update job' })).toBeInTheDocument()
    expect(jobPosts(server.calls)[0]?.body).toEqual({
      kind: 'client.update',
      payload: { version: '1.2.10' },
      targets: { type: 'devices', device_ids: ['a', 'c'] },
    })
    expect(screen.getByText('1.2.10')).toBeInTheDocument()
  })

  it('has nothing to update when no device is outdated', async () => {
    fakeCoordinator({ devices: [devices[1]!], updates: emptyUpdates({ latest: release('1.2.10'), cached: [release('1.2.10')] }) })
    start('/updates')
    expect(await screen.findByText('No device has an update available.')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /Update all outdated devices/ })).toBeDisabled()
  })

  it('copes with a coordinator that has no update support', async () => {
    fakeCoordinator({ updates: null })
    start('/updates')
    expect(await screen.findByRole('alert')).toBeInTheDocument()
  })
})

describe('Update client from the device list', () => {
  it('updates the selected devices, offering a downgrade only for a version older than a selected device', async () => {
    const latest = release('1.2.10')
    const server = fakeCoordinator({ devices, updates: emptyUpdates({ latest, cached: [latest, release('1.2.3')] }) })
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Update client…' }))

    const pick = await screen.findByRole('combobox', { name: 'Install version' })
    // LAB-01 runs 1.2.9: 1.2.10 is an upgrade, so no downgrade checkbox.
    expect(screen.queryByRole('checkbox', { name: /Allow downgrade/ })).not.toBeInTheDocument()
    await user.selectOptions(pick, '1.2.3')
    await user.click(screen.getByRole('checkbox', { name: /Allow downgrade/ }))
    await user.click(screen.getByRole('button', { name: 'Update 1 device to 1.2.3…' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Update' }))
    await waitFor(() => expect(jobPosts(server.calls)).toHaveLength(1))
    expect(jobPosts(server.calls)[0]?.body).toEqual({
      kind: 'client.update',
      payload: { version: '1.2.3', downgrade: true },
      targets: { type: 'devices', device_ids: ['a'] },
    })
  })

  it('updates a group', async () => {
    const latest = release('1.2.10')
    const server = fakeCoordinator({
      devices,
      groups: [group('g1', 'Lab', ['a', 'b'])],
      updates: emptyUpdates({ latest, cached: [latest] }),
    })
    const user = start('/update')
    await user.selectOptions(await screen.findByRole('combobox', { name: 'Use a group' }), 'Lab (2)')
    expect(screen.getByText('Group “Lab” (2 devices now)')).toBeInTheDocument()
    await user.click(await screen.findByRole('button', { name: 'Update 2 devices to 1.2.10…' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Update' }))
    await waitFor(() => expect(jobPosts(server.calls)).toHaveLength(1))
    expect(jobPosts(server.calls)[0]?.body).toMatchObject({ targets: { type: 'group', group_id: 'g1' } })
  })

  it('shows the coordinator’s refusal when the version is no longer cached', async () => {
    const latest = release('1.2.10')
    const server = fakeCoordinator({ devices, updates: emptyUpdates({ latest, cached: [latest] }) })
    server.override = (call) =>
      call.path === '/jobs' && call.method === 'POST' ? apiError(400, 'invalid_payload', 'version 1.2.10 is not cached') : undefined
    const user = start('/update', )
    await user.click(await screen.findByRole('button', { name: 'Use all devices instead' }))
    await user.click(await screen.findByRole('button', { name: /^Update 3 devices/ }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Update' }))
    expect(await within(screen.getByRole('dialog')).findByRole('alert')).toHaveTextContent('version 1.2.10 is not cached')
  })

  it('runs a client update job again with the same version', async () => {
    const latest = release('1.2.10')
    fakeCoordinator({
      devices,
      updates: emptyUpdates({ latest, cached: [latest, release('1.2.3')] }),
      jobs: { j1: job('j1', [target('t1', 'a')], { kind: 'client.update', payload: { version: '1.2.3' } }) },
    })
    const user = start('/jobs/j1')
    expect(await screen.findByRole('heading', { name: 'Client update job' })).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Run again' }))
    expect(await screen.findByRole('combobox', { name: 'Install version' })).toHaveValue('1.2.3')
  })

  it('lists a client update job by its one-line preview', async () => {
    fakeCoordinator({
      devices,
      jobs: { j1: job('j1', [target('t1', 'a')], { kind: 'client.update', payload: { version: '1.2.3' } }) },
    })
    start('/jobs')
    const link = await screen.findByRole('link', { name: 'Update client to 1.2.3' })
    expect(within(link.closest('tr') as HTMLElement).getByText('Client update')).toBeInTheDocument()
  })
})
