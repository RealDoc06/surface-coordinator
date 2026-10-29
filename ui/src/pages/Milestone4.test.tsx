import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { App } from '../App'
import type { Setting } from '../api/types'
import { callsTo, fakeCoordinator, group, jobPosts, script } from '../test/coordinator'
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

const nav = () => screen.getByRole('navigation', { name: 'Main' })

describe('navigation', () => {
  it('keeps Devices and Jobs up front and puts set-up pages in an Administration menu', async () => {
    fakeCoordinator()
    const user = start()
    await screen.findByRole('link', { name: 'LAB-01' })
    for (const name of ['Devices', 'Jobs', 'Groups', 'Scripts', 'Updates']) {
      expect(within(nav()).getByRole('link', { name })).toBeInTheDocument()
    }
    expect(within(nav()).queryByRole('link', { name: 'Enrollment keys' })).not.toBeInTheDocument()

    await user.click(within(nav()).getByRole('button', { name: 'Administration' }))
    expect(within(nav()).getByRole('menuitem', { name: 'Enrollment keys' })).toHaveAttribute('href', '/keys')
    expect(within(nav()).getByRole('menuitem', { name: 'Rejected connections' })).toHaveAttribute('href', '/rejected')
    expect(within(nav()).queryByRole('menuitem', { name: 'Accounts' })).not.toBeInTheDocument()
    expect(within(nav()).queryByRole('menuitem', { name: 'Hotspot' })).not.toBeInTheDocument()
  })

  it('lists Accounts and Hotspot when the coordinator offers them, and marks the menu on those pages', async () => {
    fakeCoordinator({
      session: {
        authenticated: true,
        auth_mode: 'accounts',
        account: { id: 'x', username: 'admin' },
        capabilities: { accounts: true, hotspot: true },
        version: '0.5.0',
        protocol_version: 3,
      },
    })
    const user = start('/keys')
    const menu = await screen.findByRole('button', { name: 'Administration' })
    expect(menu).toHaveClass('active')
    await user.click(menu)
    expect(screen.getByRole('menuitem', { name: 'Accounts' })).toBeInTheDocument()
    expect(screen.getByRole('menuitem', { name: 'Hotspot' })).toBeInTheDocument()
  })

  it('does not mark the menu on a primary page', async () => {
    fakeCoordinator()
    start('/jobs')
    expect(await screen.findByRole('button', { name: 'Administration' })).not.toHaveClass('active')
  })
})

describe('Groups page', () => {
  it('lists the groups with their member counts', async () => {
    fakeCoordinator({ groups: [group('g1', 'Front desk', ['a']), group('g2', 'Lab', ['a', 'b', 'c'])] })
    start('/groups')
    const lab = (await screen.findByRole('link', { name: 'Lab' })).closest('tr') as HTMLElement
    expect(within(lab).getByText('3')).toBeInTheDocument()
    const front = screen.getByRole('link', { name: 'Front desk' }).closest('tr') as HTMLElement
    expect(within(front).getByText('1')).toBeInTheDocument()
  })

  it('creates a group and shows it', async () => {
    const server = fakeCoordinator()
    const user = start('/groups')
    await screen.findByText('No groups yet')
    await user.type(screen.getByRole('textbox', { name: 'Name' }), '  Room 4 ')
    await user.click(screen.getByRole('button', { name: 'Create group' }))

    expect(await screen.findByRole('link', { name: 'Room 4' })).toBeInTheDocument()
    expect(callsTo(server.calls, 'POST', '/groups')[0]?.body).toEqual({ name: 'Room 4' })
    expect(screen.getByRole('textbox', { name: 'Name' })).toHaveValue('')
  })

  it('reports a name that is taken, and an empty one, without leaving the page', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab')] })
    const user = start('/groups')
    await screen.findByRole('link', { name: 'Lab' })

    await user.click(screen.getByRole('button', { name: 'Create group' }))
    expect(screen.getByRole('alert')).toHaveTextContent('Give the group a name.')
    expect(callsTo(server.calls, 'POST', '/groups')).toHaveLength(0)

    await user.type(screen.getByRole('textbox', { name: 'Name' }), 'LAB')
    await user.click(screen.getByRole('button', { name: 'Create group' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('A group named “LAB” already exists.')
  })

  it('renames a group, reporting a clash inline', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Front desk'), group('g2', 'Lab')] })
    const user = start('/groups')
    await user.click(await screen.findByRole('button', { name: 'Rename Lab' }))
    const dialog = screen.getByRole('dialog', { name: 'Rename group' })
    const input = within(dialog).getByRole('textbox', { name: 'Name' })
    expect(input).toHaveValue('Lab')

    await user.clear(input)
    await user.type(input, 'front DESK')
    await user.click(within(dialog).getByRole('button', { name: 'Rename' }))
    expect(await within(dialog).findByRole('alert')).toHaveTextContent('already exists')

    await user.clear(input)
    await user.type(input, 'Classroom')
    await user.click(within(dialog).getByRole('button', { name: 'Rename' }))
    expect(await screen.findByRole('link', { name: 'Classroom' })).toBeInTheDocument()
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(callsTo(server.calls, 'PATCH', '/groups/g2').map((c) => c.body)).toEqual([
      { name: 'front DESK' },
      { name: 'Classroom' },
    ])
  })

  it('words the delete warning for a group with one device in the singular', async () => {
    fakeCoordinator({ groups: [group('g1', 'Lab', ['a'])] })
    const user = start('/groups')
    await user.click(await screen.findByRole('button', { name: 'Delete Lab' }))
    const dialog = screen.getByRole('dialog', { name: 'Delete group “Lab”?' })
    expect(within(dialog).getByText(/its 1 device stays enrolled/)).toBeInTheDocument()
  })

  it('asks before deleting a group, and deletes it when confirmed', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab', ['a', 'b'])] })
    const user = start('/groups')
    await user.click(await screen.findByRole('button', { name: 'Delete Lab' }))
    const dialog = screen.getByRole('dialog', { name: 'Delete group “Lab”?' })
    expect(within(dialog).getByText(/its 2 devices stay enrolled/)).toBeInTheDocument()

    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }))
    expect(callsTo(server.calls, 'DELETE', '/groups/g1')).toHaveLength(0)

    await user.click(screen.getByRole('button', { name: 'Delete Lab' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete group' }))
    await screen.findByText('No groups yet')
    expect(callsTo(server.calls, 'DELETE', '/groups/g1')).toHaveLength(1)
  })

  it('edits membership with a filtered checklist and replaces the set', async () => {
    const server = fakeCoordinator({
      groups: [group('g1', 'Lab', ['a'])],
      devices: [device('a', { hostname: 'LAB-01' }), device('b', { hostname: 'LAB-02' }), device('c', { hostname: 'OFFICE-01' })],
    })
    const user = start('/groups')
    await user.click(await screen.findByRole('link', { name: 'Lab' }))

    expect(await screen.findByRole('heading', { name: 'Lab' })).toBeInTheDocument()
    expect(screen.getByRole('checkbox', { name: 'LAB-01' })).toBeChecked()
    expect(screen.getByRole('checkbox', { name: 'LAB-02' })).not.toBeChecked()
    expect(screen.getByRole('button', { name: 'Save members' })).toBeDisabled()

    await user.type(screen.getByRole('searchbox'), 'office')
    expect(screen.queryByRole('checkbox', { name: 'LAB-02' })).not.toBeInTheDocument()
    await user.click(screen.getByRole('checkbox', { name: 'OFFICE-01' }))
    await user.clear(screen.getByRole('searchbox'))
    await user.click(screen.getByRole('checkbox', { name: 'LAB-01' }))
    expect(screen.getByText('1 selected')).toBeInTheDocument()

    await user.click(screen.getByRole('button', { name: 'Save members' }))
    await waitFor(() => expect(callsTo(server.calls, 'PUT', '/groups/g1/members')).toHaveLength(1))
    expect(callsTo(server.calls, 'PUT', '/groups/g1/members')[0]?.body).toEqual({ device_ids: ['c'] })
    expect(await screen.findByText('Saved.')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Save members' })).toBeDisabled()
  })

  it('reverts unsaved membership changes', async () => {
    fakeCoordinator({ groups: [group('g1', 'Lab', ['a'])] })
    const user = start('/groups/g1')
    await user.click(await screen.findByRole('checkbox', { name: 'LAB-02' }))
    expect(screen.getByText('2 selected')).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Revert' }))
    expect(screen.getByText('1 selected')).toBeInTheDocument()
  })

  it('says when a group is gone', async () => {
    fakeCoordinator({ groups: [] })
    start('/groups/nope')
    expect(await screen.findByText('Group not found')).toBeInTheDocument()
  })

  it('runs a script on a group: summary names the group and a job goes to the group', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab', ['a', 'b'])] })
    const user = start('/groups')
    await user.click(await screen.findByRole('button', { name: 'Run on group Lab' }))

    expect(await screen.findByRole('heading', { name: 'Run script' })).toBeInTheDocument()
    expect(screen.getByText('Group “Lab” (2 devices now)')).toBeInTheDocument()
    expect(screen.getByText(/Membership is taken at the moment the job is created/)).toBeInTheDocument()
    await user.type(screen.getByRole('textbox', { name: 'Script body' }), 'hostname')
    await user.click(screen.getByRole('button', { name: 'Run on 2 devices' }))

    await waitFor(() => expect(jobPosts(server.calls)).toHaveLength(1))
    expect(jobPosts(server.calls)[0]?.body).toMatchObject({
      kind: 'script.run',
      targets: { type: 'group', group_id: 'g1' },
    })
    expect(await screen.findByRole('heading', { name: 'Script job' })).toBeInTheDocument()
  })

  it('shows an empty group’s refusal from the coordinator', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Empty')] })
    const user = start('/groups/g1')
    await user.click(await screen.findByRole('button', { name: /Run on group/ }))
    await user.type(await screen.findByRole('textbox', { name: 'Script body' }), 'x')
    await user.click(screen.getByRole('button', { name: 'Run' }))
    // Nothing to send to: caught before the request.
    expect(await screen.findByText('That group has no devices (or no longer exists).')).toBeInTheDocument()
    expect(jobPosts(server.calls)).toHaveLength(0)
  })
})

describe('groups in the device list', () => {
  it('shows each device’s groups, filters by group and runs on the filtered group', async () => {
    const server = fakeCoordinator({
      groups: [group('g1', 'Lab', ['a', 'b'])],
      devices: [
        device('a', { hostname: 'LAB-01', group_ids: ['g1'] }),
        device('b', { hostname: 'LAB-02', group_ids: ['g1'] }),
        device('c', { hostname: 'OFFICE-01', group_ids: [] }),
      ],
    })
    const user = start()
    const row = (await screen.findByRole('link', { name: 'LAB-01' })).closest('tr') as HTMLElement
    expect(within(row).getByText('Lab')).toBeInTheDocument()

    await user.selectOptions(screen.getByRole('combobox', { name: 'Group' }), 'Lab')
    expect(screen.queryByRole('link', { name: 'OFFICE-01' })).not.toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /Run on this group/ }))
    expect(await screen.findByText('Group “Lab” (2 devices now)')).toBeInTheDocument()
    await user.type(screen.getByRole('textbox', { name: 'Script body' }), 'hostname')
    await user.click(screen.getByRole('button', { name: 'Run on 2 devices' }))
    await waitFor(() => expect(jobPosts(server.calls)).toHaveLength(1))
    expect(jobPosts(server.calls)[0]?.body).toMatchObject({ targets: { type: 'group', group_id: 'g1' } })
  })

  it('adds the selection to a group with the merged membership, read fresh', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab', ['a'])] })
    // Someone else added "c" after this page loaded the groups.
    server.override = (call) => {
      if (call.method === 'GET' && call.path === '/groups' && callsTo(server.calls, 'GET', '/groups').length > 1) {
        server.groups[0]!.device_ids = ['a', 'c']
      }
      return undefined
    }
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-02' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Add to group…' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Add' }))

    await waitFor(() => expect(callsTo(server.calls, 'PUT', '/groups/g1/members')).toHaveLength(1))
    expect(callsTo(server.calls, 'PUT', '/groups/g1/members')[0]?.body).toEqual({ device_ids: ['a', 'c', 'b'] })
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
  })

  it('removes the selection from a group with the reduced membership', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab', ['a', 'b', 'c'])] })
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-02' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Remove from group…' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Remove' }))
    await waitFor(() => expect(callsTo(server.calls, 'PUT', '/groups/g1/members')).toHaveLength(1))
    expect(callsTo(server.calls, 'PUT', '/groups/g1/members')[0]?.body).toEqual({ device_ids: ['a', 'c'] })
  })

  it('refetches groups and devices on groups.changed, not jobs or anything else', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab', ['a'])] })
    start()
    await screen.findByRole('link', { name: 'LAB-01' })
    act(() => FakeWebSocket.latest().open())
    await waitFor(() => expect(callsTo(server.calls, 'GET', '/groups').length).toBeGreaterThanOrEqual(2))
    const groupsBefore = callsTo(server.calls, 'GET', '/groups').length
    const devicesBefore = callsTo(server.calls, 'GET', '/devices').length

    server.groups = [group('g1', 'Lab', ['a']), group('g2', 'New group', ['b'])]
    server.devices = [
      device('a', { hostname: 'LAB-01', group_ids: ['g1'] }),
      device('b', { hostname: 'LAB-02', group_ids: ['g2'] }),
      device('c', { hostname: 'LAB-03' }),
    ]
    act(() => FakeWebSocket.latest().send({ type: 'groups.changed' }))

    const row = (await screen.findByRole('link', { name: 'LAB-02' })).closest('tr') as HTMLElement
    expect(within(row).getByText('New group')).toBeInTheDocument()
    expect(callsTo(server.calls, 'GET', '/groups')).toHaveLength(groupsBefore + 1)
    expect(callsTo(server.calls, 'GET', '/devices')).toHaveLength(devicesBefore + 1)
    expect(server.calls.filter((c) => c.path.startsWith('/jobs'))).toHaveLength(0)
  })

  it('refetches groups on resync', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab', ['a'])] })
    start('/groups')
    await screen.findByRole('link', { name: 'Lab' })
    act(() => FakeWebSocket.latest().open())
    await waitFor(() => expect(callsTo(server.calls, 'GET', '/groups').length).toBeGreaterThanOrEqual(2))

    server.groups = [group('g1', 'Lab', ['a']), group('g2', 'Added elsewhere')]
    act(() => FakeWebSocket.latest().send({ type: 'resync' }))
    expect(await screen.findByRole('link', { name: 'Added elsewhere' })).toBeInTheDocument()
  })

  it('works against a coordinator without groups (404)', async () => {
    const server = fakeCoordinator()
    server.override = (call) => (call.path === '/groups' ? apiError(404, 'not_found') : undefined)
    start()
    expect(await screen.findByRole('link', { name: 'LAB-01' })).toBeInTheDocument()
    expect(screen.queryByRole('combobox', { name: 'Group' })).not.toBeInTheDocument()
  })
})

describe('script library', () => {
  const builtIn = script('b1', 'Check disk', { built_in: true, body: 'Get-Volume' })
  const mine = script('s1', 'Clean temp', { shell: 'cmd', body: 'del /q %TEMP%\\*' })

  it('lists scripts with shell and a built-in badge; built-ins cannot be deleted', async () => {
    fakeCoordinator({ scripts: [builtIn, mine] })
    start('/scripts')
    const builtInRow = (await screen.findByRole('link', { name: 'Check disk' })).closest('tr') as HTMLElement
    expect(within(builtInRow).getByText('built-in')).toBeInTheDocument()
    expect(within(builtInRow).getByText('PowerShell')).toBeInTheDocument()
    expect(within(builtInRow).queryByRole('button', { name: /^Delete/ })).not.toBeInTheDocument()
    expect(within(builtInRow).getByRole('button', { name: 'Duplicate Check disk' })).toBeInTheDocument()

    const myRow = screen.getByRole('link', { name: 'Clean temp' }).closest('tr') as HTMLElement
    expect(within(myRow).getByText('cmd')).toBeInTheDocument()
    expect(within(myRow).queryByText('built-in')).not.toBeInTheDocument()
    expect(within(myRow).getByRole('button', { name: 'Delete Clean temp' })).toBeInTheDocument()
  })

  it('shows a built-in script read-only with Duplicate and Run', async () => {
    fakeCoordinator({ scripts: [builtIn] })
    start('/scripts/b1')
    expect(await screen.findByRole('heading', { name: 'Check disk' })).toBeInTheDocument()
    expect(screen.getByRole('textbox', { name: 'Script body' })).toHaveAttribute('readonly')
    expect(screen.getByRole('textbox', { name: 'Name' })).toHaveAttribute('readonly')
    expect(screen.getByText(/Built-in scripts are read-only/)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Save changes' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Delete' })).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Duplicate' })).toBeInTheDocument()
  })

  it('duplicates a built-in script into the create form, named “… (copy)”', async () => {
    const server = fakeCoordinator({ scripts: [builtIn] })
    const user = start('/scripts')
    await user.click(await screen.findByRole('button', { name: 'Duplicate Check disk' }))

    expect(await screen.findByRole('heading', { name: 'New script' })).toBeInTheDocument()
    expect(screen.getByRole('textbox', { name: 'Name' })).toHaveValue('Check disk (copy)')
    expect(screen.getByRole('textbox', { name: 'Script body' })).toHaveValue('Get-Volume')
    await user.click(screen.getByRole('button', { name: 'Create script' }))

    expect(await screen.findByRole('link', { name: 'Check disk (copy)' })).toBeInTheDocument()
    expect(callsTo(server.calls, 'POST', '/scripts')[0]?.body).toEqual({
      name: 'Check disk (copy)',
      shell: 'powershell',
      body: 'Get-Volume',
    })
  })

  it('creates a script, checking the fields and reporting a name that is taken', async () => {
    const server = fakeCoordinator({ scripts: [mine] })
    const user = start('/scripts/new')
    await screen.findByRole('heading', { name: 'New script' })

    await user.click(screen.getByRole('button', { name: 'Create script' }))
    expect(screen.getAllByRole('alert').map((a) => a.textContent)).toEqual(['Give the script a name.', 'The script is empty.'])

    await user.type(screen.getByRole('textbox', { name: 'Name' }), 'clean TEMP')
    await user.type(screen.getByRole('textbox', { name: 'Script body' }), 'echo')
    await user.click(screen.getByRole('button', { name: 'Create script' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('A script named “clean TEMP” already exists.')

    const name = screen.getByRole('textbox', { name: 'Name' })
    await user.clear(name)
    await user.type(name, 'Echo')
    await user.click(screen.getByRole('button', { name: 'Create script' }))
    expect(await screen.findByRole('link', { name: 'Echo' })).toBeInTheDocument()
    expect(callsTo(server.calls, 'POST', '/scripts')).toHaveLength(2)
  })

  it('edits a script and saves it with PUT', async () => {
    const server = fakeCoordinator({ scripts: [mine] })
    const user = start('/scripts')
    await user.click(await screen.findByRole('link', { name: 'Clean temp' }))
    const body = await screen.findByRole('textbox', { name: 'Script body' })
    expect(screen.getByRole('button', { name: 'Save changes' })).toBeDisabled()
    await user.clear(body)
    await user.type(body, 'del /q C:\\Temp\\*')
    await user.click(screen.getByRole('button', { name: 'Save changes' }))
    expect(await screen.findByText('Saved.')).toBeInTheDocument()
    expect(callsTo(server.calls, 'PUT', '/scripts/s1')[0]?.body).toEqual({
      name: 'Clean temp',
      shell: 'cmd',
      body: 'del /q C:\\Temp\\*',
    })
  })

  it('shows the coordinator’s refusal to change a built-in script', async () => {
    // A stale page could still try: the server answers 403 built_in.
    const server = fakeCoordinator({ scripts: [mine] })
    server.override = (call) => (call.method === 'PUT' ? apiError(403, 'built_in') : undefined)
    const user = start('/scripts/s1')
    const body = await screen.findByRole('textbox', { name: 'Script body' })
    await user.type(body, ' more')
    await user.click(screen.getByRole('button', { name: 'Save changes' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('Built-in scripts cannot be changed or deleted.')
  })

  it('deletes after confirmation', async () => {
    const server = fakeCoordinator({ scripts: [mine] })
    const user = start('/scripts')
    await user.click(await screen.findByRole('button', { name: 'Delete Clean temp' }))
    const dialog = screen.getByRole('dialog', { name: 'Delete “Clean temp”?' })
    expect(callsTo(server.calls, 'DELETE', '/scripts/s1')).toHaveLength(0)
    await user.click(within(dialog).getByRole('button', { name: 'Delete script' }))
    await screen.findByText('No scripts yet')
    expect(callsTo(server.calls, 'DELETE', '/scripts/s1')).toHaveLength(1)
  })

  it('runs a library script on all devices by default, with the usual summary', async () => {
    const server = fakeCoordinator({ scripts: [mine] })
    const user = start('/scripts')
    await user.click(await screen.findByRole('button', { name: 'Run Clean temp' }))

    expect(await screen.findByRole('heading', { name: 'Run script' })).toBeInTheDocument()
    expect(screen.getByText('All devices (3)')).toBeInTheDocument()
    expect(screen.getByRole('textbox', { name: 'Script body' })).toHaveValue('del /q %TEMP%\\*')
    expect(screen.getByRole('radio', { name: 'cmd' })).toBeChecked()
    await user.click(screen.getByRole('button', { name: 'Run on 3 devices' }))
    await waitFor(() => expect(jobPosts(server.calls)).toHaveLength(1))
    expect(jobPosts(server.calls)[0]?.body).toMatchObject({
      kind: 'script.run',
      payload: { shell: 'cmd', body: 'del /q %TEMP%\\*' },
      targets: { type: 'all' },
    })
  })

  it('carries the device selection from the device list through the library to the composer', async () => {
    fakeCoordinator({ scripts: [mine] })
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('checkbox', { name: 'Select LAB-03' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Run script from library…' }))

    expect(await screen.findByRole('heading', { name: 'Scripts' })).toBeInTheDocument()
    expect(screen.getByText('2 selected devices: LAB-01, LAB-03')).toBeInTheDocument()
    await user.click(await screen.findByRole('button', { name: 'Run Clean temp' }))
    expect(await screen.findByText('2 selected devices: LAB-01, LAB-03')).toBeInTheDocument()
    expect(screen.getByRole('textbox', { name: 'Script body' })).toHaveValue('del /q %TEMP%\\*')
  })

  it('keeps selected devices when inspecting, editing and running a library script', async () => {
    const server = fakeCoordinator({ scripts: [mine] })
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('checkbox', { name: 'Select LAB-03' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Run script from library…' }))
    await user.click(await screen.findByRole('link', { name: 'Clean temp' }))
    const body = await screen.findByRole('textbox', { name: 'Script body' })
    await user.clear(body)
    await user.type(body, 'echo selected devices')
    await user.click(screen.getByRole('button', { name: 'Save changes' }))
    await screen.findByText('Saved.')
    await user.click(screen.getByRole('button', { name: 'Run…' }))

    expect(await screen.findByText('2 selected devices: LAB-01, LAB-03')).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Run on 2 devices' }))
    await screen.findByRole('heading', { name: 'Script job' })
    expect(jobPosts(server.calls)[0]?.body).toMatchObject({
      payload: { shell: 'cmd', body: 'echo selected devices' },
      targets: { type: 'devices', device_ids: ['a', 'c'] },
    })
  })

  it('keeps selected devices through duplicating a script and returning to the library', async () => {
    const server = fakeCoordinator({ scripts: [builtIn] })
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-02' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Run script from library…' }))
    await user.click(await screen.findByRole('link', { name: 'Check disk' }))
    await user.click(await screen.findByRole('button', { name: 'Duplicate' }))
    await user.click(screen.getByRole('button', { name: 'Create script' }))
    expect(await screen.findByText('1 selected device: LAB-02')).toBeInTheDocument()
    await user.click(await screen.findByRole('link', { name: 'Check disk (copy)' }))
    await user.click(await screen.findByRole('link', { name: 'All scripts' }))
    expect(await screen.findByText('1 selected device: LAB-02')).toBeInTheDocument()
    await user.click(await screen.findByRole('link', { name: 'Check disk (copy)' }))
    await user.click(await screen.findByRole('button', { name: 'Close' }))
    expect(await screen.findByText('1 selected device: LAB-02')).toBeInTheDocument()
    await user.click(await screen.findByRole('button', { name: 'Run Check disk (copy)' }))
    await user.click(await screen.findByRole('button', { name: 'Run on 1 device' }))
    await screen.findByRole('heading', { name: 'Script job' })
    expect(jobPosts(server.calls)[0]?.body).toMatchObject({ targets: { type: 'devices', device_ids: ['b'] } })
  })

  it('keeps selected devices when returning from a script that was deleted elsewhere', async () => {
    const server = fakeCoordinator({ scripts: [mine, builtIn] })
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Run script from library…' }))
    const scriptLink = await screen.findByRole('link', { name: 'Clean temp' })
    server.scripts = [builtIn]
    await user.click(scriptLink)
    await screen.findByRole('heading', { name: 'Script not found' })
    await user.click(screen.getByRole('link', { name: 'Back to scripts' }))
    expect(await screen.findByText('1 selected device: LAB-01')).toBeInTheDocument()
    await user.click(await screen.findByRole('button', { name: 'Run Check disk' }))
    await user.click(await screen.findByRole('button', { name: 'Run on 1 device' }))
    await screen.findByRole('heading', { name: 'Script job' })
    expect(jobPosts(server.calls)[0]?.body).toMatchObject({ targets: { type: 'devices', device_ids: ['a'] } })
  })

  it('says when a script is not found', async () => {
    fakeCoordinator({ scripts: [] })
    start('/scripts/nope')
    expect(await screen.findByText('Script not found')).toBeInTheDocument()
  })
})

const volume: Setting = {
  id: 'audio.volume',
  label: 'Volume',
  description: 'Master volume.',
  scope: 'user',
  value: { type: 'integer', min: 0, max: 100, unit: '%' },
}

describe('the New job page', () => {
  it('switches the action and keeps the targets', async () => {
    fakeCoordinator()
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-02' }))
    await user.click(screen.getByRole('button', { name: /^Run script/ }))
    expect(await screen.findByText('1 selected device: LAB-02')).toBeInTheDocument()

    const actions = screen.getByRole('navigation', { name: 'Action' })
    expect(within(actions).getAllByRole('link').map((a) => a.textContent)).toEqual([
      'Run script',
      'Launch app',
      'Apply setting',
      'Power',
      'Move to Wi-Fi',
      'Update client',
    ])
    await user.click(within(actions).getByRole('link', { name: 'Launch app' }))
    expect(await screen.findByRole('heading', { name: 'Launch app' })).toBeInTheDocument()
    expect(screen.getByText('1 selected device: LAB-02')).toBeInTheDocument()
    await user.click(within(actions).getByRole('link', { name: 'Power' }))
    expect(await screen.findByRole('heading', { name: 'Power' })).toBeInTheDocument()
    expect(screen.getByText('1 selected device: LAB-02')).toBeInTheDocument()
  })

  it('can target a group from the targets panel', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab', ['a', 'b'])] })
    const user = start('/launch')
    await user.type(await screen.findByRole('textbox', { name: 'Path' }), 'C:\\app.exe')
    await user.selectOptions(screen.getByRole('combobox', { name: 'Use a group' }), 'Lab (2)')
    expect(screen.getByText('Group “Lab” (2 devices now)')).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Launch on 2 devices' }))
    await waitFor(() => expect(jobPosts(server.calls)).toHaveLength(1))
    expect(jobPosts(server.calls)[0]?.body).toEqual({
      kind: 'app.launch',
      payload: { path: 'C:\\app.exe' },
      targets: { type: 'group', group_id: 'g1' },
    })
  })

  it('launches an app on the selection from the device list menu and shows the job', async () => {
    const server = fakeCoordinator()
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Launch app…' }))
    await user.type(await screen.findByRole('textbox', { name: 'Path' }), 'C:\\Apps\\app.exe')
    await user.click(screen.getByRole('button', { name: 'Add argument' }))
    await user.type(screen.getByRole('textbox', { name: 'Argument 1' }), '--kiosk')
    await user.click(screen.getByRole('button', { name: 'Launch on 1 device' }))

    expect(await screen.findByRole('heading', { name: 'Launch app job' })).toBeInTheDocument()
    expect(jobPosts(server.calls)[0]?.body).toEqual({
      kind: 'app.launch',
      payload: { path: 'C:\\Apps\\app.exe', arguments: ['--kiosk'] },
      targets: { type: 'devices', device_ids: ['a'] },
    })
    expect(screen.getByText('--kiosk')).toBeInTheDocument()
  })

  it('applies a setting chosen from the catalogue', async () => {
    const server = fakeCoordinator({ catalogue: [volume] })
    const user = start('/setting')
    await user.click(await screen.findByRole('button', { name: 'Use all devices instead' }))
    await user.selectOptions(await screen.findByRole('combobox', { name: 'Setting' }), 'Volume')
    const number = screen.getByRole('spinbutton', { name: 'Value (%)' })
    await user.clear(number)
    await user.type(number, '40')
    await user.click(screen.getByRole('button', { name: 'Apply to 3 devices' }))

    expect(await screen.findByRole('heading', { name: 'Setting job' })).toBeInTheDocument()
    expect(jobPosts(server.calls)[0]?.body).toEqual({
      kind: 'setting.apply',
      payload: { setting: 'audio.volume', value: 40 },
      targets: { type: 'all' },
    })
    // The page shows the catalogue label and unit.
    expect(screen.getByText('Volume')).toBeInTheDocument()
    expect(screen.getByText('40 %')).toBeInTheDocument()
  })

  it('restarts devices only after confirmation, from the device page', async () => {
    const server = fakeCoordinator()
    const user = start('/devices/b')
    await user.click(await screen.findByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: /Restart, shut down or sleep/ }))
    await user.click(await screen.findByRole('radio', { name: /Restart/ }))
    await user.click(screen.getByRole('button', { name: 'Restart 1 device…' }))
    expect(jobPosts(server.calls)).toHaveLength(0)
    await user.click(within(screen.getByRole('dialog', { name: 'Restart 1 device?' })).getByRole('button', { name: 'Restart' }))

    expect(await screen.findByRole('heading', { name: 'Power job' })).toBeInTheDocument()
    expect(jobPosts(server.calls)[0]?.body).toEqual({
      kind: 'power',
      payload: { action: 'restart' },
      targets: { type: 'devices', device_ids: ['b'] },
    })
  })

  it('lists new kinds with one-line previews and runs them again', async () => {
    const server = fakeCoordinator({
      catalogue: [volume],
      jobs: {
        j1: job('j1', [target('t1', 'a')], { kind: 'setting.apply', payload: { setting: 'audio.volume', value: 40 }, created_at: 3000 }),
        j2: job('j2', [target('t2', 'a')], { kind: 'app.launch', payload: { path: 'C:\\x\\app.exe' }, created_at: 2000 }),
        j3: job('j3', [target('t3', 'a')], { kind: 'power', payload: { action: 'restart' }, created_at: 1000 }),
      },
    })
    const user = start('/jobs')
    expect(await screen.findByRole('link', { name: 'Set Volume to 40 %' })).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Launch C:\\x\\app.exe' })).toBeInTheDocument()

    await user.click(screen.getByRole('link', { name: 'Restart' }))
    await user.click(await screen.findByRole('button', { name: 'Run again' }))
    expect(await screen.findByRole('heading', { name: 'Power' })).toBeInTheDocument()
    expect(screen.getByRole('radio', { name: /Restart/ })).toBeChecked()
    expect(screen.getByText('1 selected device: LAB-01')).toBeInTheDocument()
    expect(jobPosts(server.calls)).toHaveLength(0)
  })

  it('shows jobs of a kind it does not know generically, and ignores unknown events', async () => {
    fakeCoordinator({
      jobs: { jx: job('jx', [target('t1', 'a')], { kind: 'quantum.entangle', payload: { qubits: 2 } }) },
    })
    start('/jobs')
    expect(await screen.findByRole('link', { name: '{"qubits":2}' })).toBeInTheDocument()
    expect(screen.getByText('quantum.entangle')).toBeInTheDocument()
    act(() => FakeWebSocket.latest().open())
    act(() => FakeWebSocket.latest().send({ type: 'quantum.event', x: 1 }))
    expect(screen.getByRole('link', { name: '{"qubits":2}' })).toBeInTheDocument()
  })
})

describe('cancelling a job', () => {
  const running = () =>
    job('j1', [
      target('t1', 'a', { state: 'running', started_at: 6000 }),
      target('t2', 'b', { state: 'queued' }),
      target('t3', 'c', { state: 'succeeded', exit_code: 0, started_at: 6000, finished_at: 7000 }),
    ])

  it('offers Cancel job while a target is unfinished, confirms, and shows cancelled targets neutrally', async () => {
    const server = fakeCoordinator({ jobs: { j1: running() } })
    const user = start('/jobs/j1')
    await user.click(await screen.findByRole('button', { name: 'Cancel job' }))

    const dialog = screen.getByRole('dialog', { name: 'Cancel this job?' })
    expect(within(dialog).getByText(/2 devices have not finished/)).toBeInTheDocument()
    expect(callsTo(server.calls, 'POST', '/jobs/j1/cancel')).toHaveLength(0)
    await user.click(within(dialog).getByRole('button', { name: 'Cancel job' }))

    await waitFor(() => expect(callsTo(server.calls, 'POST', '/jobs/j1/cancel')).toHaveLength(1))
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
    const row = screen.getByRole('row', { name: /LAB-01/ })
    const badge = within(row).getByText('cancelled', { selector: '.badge' })
    expect(badge).toHaveClass('state-cancelled')
    expect(badge).not.toHaveClass('state-failed')
    expect(within(row).getByText('Cancelled')).toBeInTheDocument()
    expect(within(screen.getByRole('row', { name: /LAB-03/ })).getByText('succeeded')).toBeInTheDocument()
    expect(screen.getByText('2 cancelled', { selector: '.count' })).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Cancel job' })).not.toBeInTheDocument()
  })

  it('keeps the job when the confirmation is dismissed', async () => {
    const server = fakeCoordinator({ jobs: { j1: running() } })
    const user = start('/jobs/j1')
    await user.click(await screen.findByRole('button', { name: 'Cancel job' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Cancel' }))
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(callsTo(server.calls, 'POST', '/jobs/j1/cancel')).toHaveLength(0)
    expect(screen.getByRole('button', { name: 'Cancel job' })).toBeInTheDocument()
  })

  it('is not offered once every target has finished', async () => {
    fakeCoordinator({
      jobs: {
        j1: job('j1', [
          target('t1', 'a', { state: 'succeeded', exit_code: 0, started_at: 1, finished_at: 2 }),
          target('t2', 'b', { state: 'failed', reason: 'timeout', started_at: 1, finished_at: 2 }),
        ]),
      },
    })
    start('/jobs/j1')
    await screen.findByRole('row', { name: /LAB-01/ })
    expect(screen.queryByRole('button', { name: 'Cancel job' })).not.toBeInTheDocument()
  })

  it('applies the target updates the coordinator announces', async () => {
    fakeCoordinator({ jobs: { j1: running() } })
    start('/jobs/j1')
    await screen.findByRole('button', { name: 'Cancel job' })
    act(() => FakeWebSocket.latest().open())
    act(() =>
      FakeWebSocket.latest().send({
        type: 'job.target.updated',
        job_id: 'j1',
        target: target('t1', 'a', { state: 'failed', reason: 'cancelled', started_at: 6000, finished_at: 6500 }),
      }),
    )
    expect(within(screen.getByRole('row', { name: /LAB-01/ })).getByText('cancelled', { selector: '.badge' })).toHaveClass('state-cancelled')
    // t2 is still queued, so the button stays.
    expect(screen.getByRole('button', { name: 'Cancel job' })).toBeInTheDocument()
  })

  it('shows the reason when the coordinator will not cancel', async () => {
    const server = fakeCoordinator({ jobs: { j1: running() } })
    server.override = (call) => (call.path === '/jobs/j1/cancel' ? apiError(500, 'internal', 'could not reach the dispatcher') : undefined)
    const user = start('/jobs/j1')
    await user.click(await screen.findByRole('button', { name: 'Cancel job' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Cancel job' }))
    expect(await within(screen.getByRole('dialog')).findByRole('alert')).toHaveTextContent('could not reach the dispatcher')
  })

  it('words every failure reason, including the new ones', async () => {
    fakeCoordinator({
      devices: [1, 2, 3, 4, 5, 6, 7].map((i) => device(`d${i}`, { hostname: `HOST-${i}` })),
      jobs: {
        j1: job(
          'j1',
          [
            target('t1', 'd1', { state: 'failed', reason: 'no_user_session' }),
            target('t2', 'd2', { state: 'failed', reason: 'bad_signature' }),
            target('t3', 'd3', { state: 'failed', reason: 'bad_hash' }),
            target('t4', 'd4', { state: 'failed', reason: 'downgrade_refused' }),
            target('t5', 'd5', { state: 'failed', reason: 'download_failed', message: 'HTTP 404' }),
            target('t6', 'd6', { state: 'failed', reason: 'rolled_back' }),
            target('t7', 'd7', { state: 'failed', reason: 'something_new' }),
          ],
          { kind: 'client.update', payload: { version: '1.2.3' } },
        ),
      },
    })
    start('/jobs/j1')
    const cell = async (host: string) => within(await screen.findByRole('row', { name: new RegExp(host) })).getAllByRole('cell')[4]
    expect(await cell('HOST-1')).toHaveTextContent('Nobody is logged in on the device')
    expect(await cell('HOST-2')).toHaveTextContent('signature did not verify')
    expect(await cell('HOST-3')).toHaveTextContent('did not match its checksum')
    expect(await cell('HOST-4')).toHaveTextContent('refused to install an older version')
    expect(await cell('HOST-5')).toHaveTextContent('could not download the update: HTTP 404')
    expect(await cell('HOST-6')).toHaveTextContent('went back to its previous version')
    expect(await cell('HOST-7')).toHaveTextContent('something_new')
  })
})
