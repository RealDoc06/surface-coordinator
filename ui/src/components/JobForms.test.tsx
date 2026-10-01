import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Device, Group, Job, Setting, TargetSpec, Updates } from '../api/types'
import { emptyUpdates, release } from '../test/coordinator'
import { apiError, device, job, mockFetch, target, type Call } from '../test/helpers'
import { ClientUpdateForm } from './ClientUpdateForm'
import { LaunchAppForm } from './LaunchAppForm'
import { PowerForm } from './PowerForm'
import { SettingForm } from './SettingForm'
import { TargetSummary } from './TargetSummary'

const devices: Device[] = [
  device('a', { hostname: 'LAB-01', client_version: '1.2.9' }),
  device('b', { hostname: 'LAB-02', client_version: '1.2.10' }),
  device('c', { hostname: 'LAB-03', client_version: 'dev-build', online: false }),
]
const groups: Group[] = [{ id: 'g1', name: 'Lab', created_at: 1, device_ids: ['a', 'b', 'c'] }]
const created = job('job-1', [target('t1', 'a')])
const all: TargetSpec = { type: 'devices', device_ids: ['a', 'b', 'c'] }

const jobPosts = (calls: Call[]) => calls.filter((c) => c.method === 'POST' && c.path === '/jobs')

afterEach(() => {
  vi.unstubAllGlobals()
})

function common(overrides: { targets?: TargetSpec | null; devices?: Device[] } = {}) {
  const onCreated = vi.fn<(job: Job) => void>()
  return {
    onCreated,
    props: {
      targets: overrides.targets === undefined ? all : overrides.targets,
      devices: overrides.devices ?? devices,
      groups,
      onCreated,
    },
  }
}

describe('LaunchAppForm', () => {
  function setup(options: { initial?: { path?: string; arguments?: string[] }; reply?: Parameters<typeof mockFetch>[0]; targets?: TargetSpec | null } = {}) {
    const calls = mockFetch(options.reply ?? (() => ({ status: 201, body: { job: created } })))
    const { onCreated, props } = common({ targets: options.targets })
    const user = userEvent.setup()
    render(
      <MemoryRouter>
        <LaunchAppForm {...props} initial={options.initial} />
      </MemoryRouter>,
    )
    return { calls, onCreated, user }
  }
  const submit = () => screen.getByRole('button', { name: /^Launch on/ })

  it('explains that the app starts in the logged-in user’s session', () => {
    setup()
    expect(screen.getByText(/session of the user who is logged in/)).toBeInTheDocument()
    expect(screen.getByText(/no user session/)).toBeInTheDocument()
  })

  it('requires a path', async () => {
    const { calls, user } = setup()
    await user.click(submit())
    expect(screen.getByRole('alert')).toHaveTextContent('Enter the path of the program to start.')
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('creates the job with the path and the arguments in order, ignoring blank rows', async () => {
    const { calls, onCreated, user } = setup()
    await user.type(screen.getByRole('textbox', { name: 'Path' }), 'C:\\Apps\\app.exe')
    await user.click(screen.getByRole('button', { name: 'Add argument' }))
    await user.type(screen.getByRole('textbox', { name: 'Argument 1' }), '--fast')
    await user.click(screen.getByRole('button', { name: 'Add argument' }))
    await user.click(screen.getByRole('button', { name: 'Add argument' }))
    await user.type(screen.getByRole('textbox', { name: 'Argument 3' }), 'two words')
    await user.click(submit())

    await waitFor(() => expect(onCreated).toHaveBeenCalledExactlyOnceWith(created))
    expect(jobPosts(calls)[0]?.body).toEqual({
      kind: 'app.launch',
      payload: { path: 'C:\\Apps\\app.exe', arguments: ['--fast', 'two words'] },
      targets: { type: 'devices', device_ids: ['a', 'b', 'c'] },
    })
  })

  it('moves focus to a new argument row and removes the one asked for', async () => {
    const { user } = setup({ initial: { path: 'x', arguments: ['one', 'two'] } })
    await user.click(screen.getByRole('button', { name: 'Add argument' }))
    expect(screen.getByRole('textbox', { name: 'Argument 3' })).toHaveFocus()
    await user.click(screen.getByRole('button', { name: 'Remove argument 1' }))
    expect(screen.getAllByRole('textbox', { name: /^Argument/ }).map((i) => (i as HTMLInputElement).value)).toEqual(['two', ''])
  })

  it('stops adding at 64 arguments', async () => {
    setup({ initial: { path: 'x', arguments: Array.from({ length: 64 }, (_, i) => `a${i}`) } })
    expect(screen.getByRole('button', { name: 'Add argument' })).toBeDisabled()
  })

  it('rejects a path or an argument that is too long, without sending', async () => {
    const { calls, user } = setup({ initial: { path: 'p'.repeat(1025), arguments: ['ok', 'b'.repeat(1025)] } })
    await user.click(submit())
    const alerts = screen.getAllByRole('alert').map((a) => a.textContent)
    expect(alerts).toEqual([
      'The path can be at most 1024 characters (it is 1025).',
      'An argument can be at most 1024 characters (this one is 1025).',
    ])
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('shows what the coordinator says when it refuses', async () => {
    const { user } = setup({ initial: { path: 'x' }, reply: () => apiError(400, 'invalid_payload', 'path must not contain NUL') })
    await user.click(submit())
    expect(await screen.findByRole('alert')).toHaveTextContent('path must not contain NUL')
  })

  it('sends a group as the group', async () => {
    const { calls, user } = setup({ initial: { path: 'x' }, targets: { type: 'group', group_id: 'g1' } })
    expect(screen.getByText(/Group “Lab” \(3 devices now\)/)).toBeInTheDocument()
    await user.click(submit())
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ targets: { type: 'group', group_id: 'g1' } })
  })

  it('does not send when there is nothing selected', async () => {
    const { calls, user } = setup({ initial: { path: 'x' }, targets: { type: 'devices', device_ids: [] } })
    await user.click(screen.getByRole('button', { name: 'Launch' }))
    expect(screen.getByRole('alert')).toHaveTextContent('Select at least one device to launch the app on.')
    expect(jobPosts(calls)).toHaveLength(0)
  })
})

const volume: Setting = {
  id: 'audio.volume',
  label: 'Volume',
  description: 'The master volume of the device.',
  scope: 'user',
  value: { type: 'integer', min: 0, max: 100, unit: '%' },
}
const sleep: Setting = {
  id: 'power.sleep_timeout',
  label: 'Sleep timeout',
  description: 'Minutes of inactivity before sleep.',
  scope: 'machine',
  value: { type: 'integer', min: 1, max: 720, unit: 'minutes' },
}
const dark: Setting = {
  id: 'ui.dark_mode',
  label: 'Dark mode',
  description: 'Use the dark theme.',
  scope: 'user',
  value: { type: 'bool' },
}
const wallpaper: Setting = {
  id: 'ui.wallpaper',
  label: 'Wallpaper',
  description: 'Path of the picture on the device.',
  scope: 'user',
  value: { type: 'text', max_len: 12 },
}
const catalogue = [volume, sleep, dark, wallpaper]

describe('SettingForm', () => {
  function setup(options: { catalogue?: Setting[] | null; initial?: { setting?: string; value?: boolean | number | string }; reply?: Parameters<typeof mockFetch>[0] } = {}) {
    const calls = mockFetch(options.reply ?? (() => ({ status: 201, body: { job: created } })))
    const { onCreated, props } = common()
    const user = userEvent.setup()
    render(
      <MemoryRouter>
        <SettingForm {...props} catalogue={options.catalogue === undefined ? catalogue : options.catalogue} initial={options.initial} />
      </MemoryRouter>,
    )
    return { calls, onCreated, user }
  }
  const pick = (user: ReturnType<typeof userEvent.setup>, label: string) =>
    user.selectOptions(screen.getByRole('combobox', { name: 'Setting' }), label)
  const submit = () => screen.getByRole('button', { name: /^Apply/ })

  it('asks for a setting first', async () => {
    const { calls, user } = setup()
    await user.click(submit())
    expect(screen.getByRole('alert')).toHaveTextContent('Choose a setting.')
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('lists the catalogue by label and shows the description and the scope note', async () => {
    const { user } = setup()
    expect(within(screen.getByRole('combobox', { name: 'Setting' })).getAllByRole('option').map((o) => o.textContent)).toEqual([
      'Choose a setting…',
      'Volume',
      'Sleep timeout',
      'Dark mode',
      'Wallpaper',
    ])
    await pick(user, 'Volume')
    expect(screen.getByText('The master volume of the device.')).toBeInTheDocument()
    expect(screen.getByText(/someone has to be logged in on the device/)).toBeInTheDocument()
    await pick(user, 'Sleep timeout')
    expect(screen.queryByText(/someone has to be logged in/)).not.toBeInTheDocument()
    expect(screen.getByText(/applies to the whole machine/)).toBeInTheDocument()
  })

  it('edits a percent with a slider and a number that follow each other', async () => {
    const { calls, user } = setup()
    await pick(user, 'Volume')
    const slider = screen.getByRole('slider', { name: 'Volume slider' })
    const number = screen.getByRole('spinbutton', { name: 'Value (%)' })
    expect(number).toHaveValue(0)
    await user.clear(number)
    await user.type(number, '40')
    expect(slider).toHaveValue('40')
    await user.click(submit())
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toEqual({
      kind: 'setting.apply',
      payload: { setting: 'audio.volume', value: 40 },
      targets: { type: 'devices', device_ids: ['a', 'b', 'c'] },
    })
  })

  it('has a number box only, with min, max and unit, for other integers', async () => {
    const { user } = setup()
    await pick(user, 'Sleep timeout')
    expect(screen.queryByRole('slider')).not.toBeInTheDocument()
    const number = screen.getByRole('spinbutton', { name: 'Value (minutes)' })
    expect(number).toHaveAttribute('min', '1')
    expect(number).toHaveAttribute('max', '720')
    expect(screen.getByText('1 to 720 minutes.')).toBeInTheDocument()
  })

  it('refuses a value out of range or not whole, without sending', async () => {
    const { calls, user } = setup()
    await pick(user, 'Sleep timeout')
    const number = screen.getByRole('spinbutton', { name: 'Value (minutes)' })
    await user.clear(number)
    await user.type(number, '721')
    await user.click(submit())
    expect(screen.getByRole('alert')).toHaveTextContent('Enter a whole number from 1 to 720 minutes.')
    await user.clear(number)
    await user.type(number, '2.5')
    await user.click(submit())
    expect(screen.getByRole('alert')).toHaveTextContent('Enter a whole number from 1 to 720 minutes.')
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('edits a bool with an on/off switch', async () => {
    const { calls, user } = setup()
    await pick(user, 'Dark mode')
    const toggle = screen.getByRole('switch', { name: 'Value' })
    expect(toggle).not.toBeChecked()
    expect(screen.getByText('Off')).toBeInTheDocument()
    await user.click(toggle)
    expect(screen.getByText('On')).toBeInTheDocument()
    await user.click(submit())
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ payload: { setting: 'ui.dark_mode', value: true } })
  })

  it('sends false for a switch left off', async () => {
    const { calls, user } = setup()
    await pick(user, 'Dark mode')
    await user.click(submit())
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ payload: { setting: 'ui.dark_mode', value: false } })
  })

  it('edits text within max_len', async () => {
    const { calls, user } = setup()
    await pick(user, 'Wallpaper')
    const input = screen.getByRole('textbox', { name: 'Value' })
    await user.type(input, 'x'.repeat(13))
    expect(screen.getByText('Up to 12 characters (13 used).')).toBeInTheDocument()
    await user.click(submit())
    expect(screen.getByRole('alert')).toHaveTextContent('At most 12 characters (this is 13).')
    expect(jobPosts(calls)).toHaveLength(0)
    await user.type(input, '{Backspace}')
    await user.click(submit())
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ payload: { setting: 'ui.wallpaper', value: 'x'.repeat(12) } })
  })

  it('starts from a previous job’s setting and value', () => {
    setup({ initial: { setting: 'audio.volume', value: 65 } })
    expect(screen.getByRole('combobox', { name: 'Setting' })).toHaveValue('audio.volume')
    expect(screen.getByRole('spinbutton', { name: 'Value (%)' })).toHaveValue(65)
  })

  it('shows the coordinator’s invalid_payload message in the form', async () => {
    const { user } = setup({
      initial: { setting: 'audio.volume', value: 10 },
      reply: () => apiError(400, 'invalid_payload', 'value 10 is not allowed for audio.volume'),
    })
    await user.click(submit())
    expect(await screen.findByRole('alert')).toHaveTextContent('value 10 is not allowed for audio.volume')
  })

  it('says when the catalogue is not available', () => {
    setup({ catalogue: null })
    expect(screen.getByText(/list of settings could not be loaded/)).toBeInTheDocument()
    expect(screen.queryByRole('combobox')).not.toBeInTheDocument()
  })
})

describe('PowerForm', () => {
  function setup(options: { initial?: { action?: 'restart' | 'shutdown' | 'sleep' }; targets?: TargetSpec | null } = {}) {
    const calls = mockFetch(() => ({ status: 201, body: { job: created } }))
    const { onCreated, props } = common({ targets: options.targets })
    const user = userEvent.setup()
    render(
      <MemoryRouter>
        <PowerForm {...props} initial={options.initial} />
      </MemoryRouter>,
    )
    return { calls, onCreated, user }
  }

  it('offers restart, shut down and sleep, none chosen', () => {
    setup()
    expect(screen.getAllByRole('radio')).toHaveLength(3)
    for (const radio of screen.getAllByRole('radio')) expect(radio).not.toBeChecked()
  })

  it('needs an action before it continues', async () => {
    const { calls, user } = setup()
    await user.click(screen.getByRole('button', { name: 'Continue…' }))
    expect(screen.getByRole('alert')).toHaveTextContent('Choose restart, shut down or sleep.')
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('confirms with the number of devices before sending anything', async () => {
    const { calls, onCreated, user } = setup()
    await user.click(screen.getByRole('radio', { name: /Restart/ }))
    await user.click(screen.getByRole('button', { name: 'Restart 3 devices…' }))

    const dialog = screen.getByRole('dialog', { name: 'Restart 3 devices?' })
    expect(jobPosts(calls)).toHaveLength(0)
    expect(within(dialog).getByRole('button', { name: 'Restart' })).not.toHaveClass('danger')
    await user.click(within(dialog).getByRole('button', { name: 'Restart' }))

    await waitFor(() => expect(onCreated).toHaveBeenCalledExactlyOnceWith(created))
    expect(jobPosts(calls)[0]?.body).toEqual({
      kind: 'power',
      payload: { action: 'restart' },
      targets: { type: 'devices', device_ids: ['a', 'b', 'c'] },
    })
  })

  it('styles shutting down as destructive and says someone has to switch the devices on again', async () => {
    const { user } = setup({ targets: { type: 'devices', device_ids: ['a'] } })
    await user.click(screen.getByRole('radio', { name: /Shut down/ }))
    await user.click(screen.getByRole('button', { name: 'Shut down 1 device…' }))
    const dialog = screen.getByRole('dialog', { name: 'Shut down 1 device?' })
    expect(within(dialog).getByRole('button', { name: 'Shut down' })).toHaveClass('danger')
    expect(within(dialog).getByText(/Someone has to switch it on again at the device/)).toBeInTheDocument()
  })

  it('sends nothing when the confirmation is cancelled', async () => {
    const { calls, user } = setup({ initial: { action: 'sleep' } })
    await user.click(screen.getByRole('button', { name: 'Put to sleep 3 devices…' }))
    await user.click(within(screen.getByRole('dialog', { name: 'Put 3 devices to sleep?' })).getByRole('button', { name: 'Cancel' }))
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('counts the members of a group', async () => {
    const { calls, user } = setup({ targets: { type: 'group', group_id: 'g1' }, initial: { action: 'restart' } })
    await user.click(screen.getByRole('button', { name: 'Restart 3 devices…' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Restart' }))
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ targets: { type: 'group', group_id: 'g1' } })
  })

  it('keeps the confirmation open and shows the refusal', async () => {
    mockFetch(() => apiError(400, 'unsupported_kind'))
    const { props } = common()
    const user = userEvent.setup()
    render(
      <MemoryRouter>
        <PowerForm {...props} initial={{ action: 'restart' }} />
      </MemoryRouter>,
    )
    await user.click(screen.getByRole('button', { name: 'Restart 3 devices…' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Restart' }))
    expect(await within(screen.getByRole('dialog')).findByRole('alert')).toHaveTextContent('does not support this kind of job')
  })

  it('warns about restricted devices, which only accept client updates', () => {
    mockFetch(() => undefined)
    const { props } = common({ devices: [device('a', { hostname: 'OLD', restricted: true }), device('b'), device('c')] })
    render(
      <MemoryRouter>
        <PowerForm {...props} />
      </MemoryRouter>,
    )
    expect(screen.getByText(/1 device is restricted/)).toBeInTheDocument()
  })
})

describe('ClientUpdateForm', () => {
  const updates = (overrides: Partial<Updates> = {}): Updates =>
    emptyUpdates({
      latest: release('1.2.10'),
      cached: [release('1.2.10'), release('1.2.3'), release('1.0.0')],
      ...overrides,
    })

  function setup(options: { updates?: Updates | null; targets?: TargetSpec | null; initial?: { version?: string; downgrade?: boolean }; devices?: Device[] } = {}) {
    const calls = mockFetch((call) => {
      if (call.path === '/updates') return options.updates === null ? undefined : { body: options.updates ?? updates() }
      if (call.path === '/jobs') return { status: 201, body: { job: created } }
      return undefined
    })
    const { onCreated, props } = common({ targets: options.targets, devices: options.devices })
    const user = userEvent.setup()
    render(
      <MemoryRouter>
        <ClientUpdateForm {...props} initial={options.initial} />
      </MemoryRouter>,
    )
    return { calls, onCreated, user }
  }
  const version = () => screen.findByRole('combobox', { name: 'Install version' })

  it('defaults to the newest cached version', async () => {
    setup()
    expect(await version()).toHaveValue('1.2.10')
    expect(within(await version()).getAllByRole('option').map((o) => o.textContent)).toEqual(['1.2.10 (newest)', '1.2.3', '1.0.0'])
  })

  it('summarises how many devices will be updated, are already on it, or are newer', async () => {
    setup({ targets: all })
    await version()
    // 1.2.9 and the unparseable "dev-build" are updated; 1.2.10 is current.
    expect(screen.getByText(/2 devices will be updated to 1\.2\.10\./)).toBeInTheDocument()
    expect(screen.getByText(/1 device is already on 1\.2\.10 and is left as it is\./)).toBeInTheDocument()
    expect(screen.queryByRole('checkbox', { name: /Allow downgrade/ })).not.toBeInTheDocument()
  })

  it('compares versions numerically: 1.2.10 is newer than 1.2.9', async () => {
    const { user } = setup({ targets: { type: 'devices', device_ids: ['a'] } })
    // The only device is on 1.2.9: nothing newer is installed, so no downgrade is needed for 1.2.10.
    expect(await version()).toHaveValue('1.2.10')
    expect(screen.queryByRole('checkbox', { name: /Allow downgrade/ })).not.toBeInTheDocument()
    await user.selectOptions(await version(), '1.2.3')
    // 1.2.3 is older than 1.2.9, so installing it is a downgrade.
    expect(screen.getByRole('checkbox', { name: /Allow downgrade/ })).not.toBeChecked()
  })

  it('shows "allow downgrade" only when the chosen version is older than a selected device', async () => {
    const { user } = setup({ targets: all })
    await version()
    expect(screen.queryByRole('checkbox', { name: /Allow downgrade/ })).not.toBeInTheDocument()
    await user.selectOptions(await version(), '1.2.3')
    expect(screen.getByRole('checkbox', { name: /Allow downgrade/ })).toBeInTheDocument()
    await user.selectOptions(await version(), '1.2.10')
    expect(screen.queryByRole('checkbox', { name: /Allow downgrade/ })).not.toBeInTheDocument()
  })

  it('never treats an unparseable device version as newer', async () => {
    const { user } = setup({ targets: { type: 'devices', device_ids: ['c'] } })
    await user.selectOptions(await version(), '1.0.0')
    expect(screen.queryByRole('checkbox', { name: /Allow downgrade/ })).not.toBeInTheDocument()
  })

  it('confirms, then sends the version without downgrade', async () => {
    const { calls, onCreated, user } = setup({ targets: all })
    await version()
    await user.click(screen.getByRole('button', { name: 'Update 3 devices to 1.2.10…' }))
    const dialog = screen.getByRole('dialog', { name: 'Update 3 devices to 1.2.10?' })
    expect(jobPosts(calls)).toHaveLength(0)
    await user.click(within(dialog).getByRole('button', { name: 'Update' }))
    await waitFor(() => expect(onCreated).toHaveBeenCalledExactlyOnceWith(created))
    expect(jobPosts(calls)[0]?.body).toEqual({
      kind: 'client.update',
      payload: { version: '1.2.10' },
      targets: { type: 'devices', device_ids: ['a', 'b', 'c'] },
    })
  })

  it('sends downgrade: true when it is allowed and needed', async () => {
    const { calls, user } = setup({ targets: all })
    await user.selectOptions(await version(), '1.0.0')
    await user.click(screen.getByRole('checkbox', { name: /Allow downgrade/ }))
    await user.click(screen.getByRole('button', { name: 'Update 3 devices to 1.0.0…' }))
    expect(screen.getByText(/Downgrade allowed/)).toBeInTheDocument()
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Update' }))
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ payload: { version: '1.0.0', downgrade: true } })
  })

  it('does not send a ticked downgrade once the version no longer needs one', async () => {
    const { calls, user } = setup({ targets: all })
    await user.selectOptions(await version(), '1.0.0')
    await user.click(screen.getByRole('checkbox', { name: /Allow downgrade/ }))
    await user.selectOptions(await version(), '1.2.10')
    await user.click(screen.getByRole('button', { name: 'Update 3 devices to 1.2.10…' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Update' }))
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ payload: { version: '1.2.10' } })
  })

  it('starts from the version of the job being repeated, if it is still cached', async () => {
    setup({ initial: { version: '1.2.3' } })
    expect(await version()).toHaveValue('1.2.3')
  })

  it('falls back to the newest when the repeated version is gone', async () => {
    setup({ initial: { version: '0.0.1' } })
    expect(await version()).toHaveValue('1.2.10')
  })

  it('says so when nothing has been downloaded', async () => {
    setup({ updates: emptyUpdates() })
    expect(await screen.findByText(/No client version has been downloaded yet/)).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Updates' })).toHaveAttribute('href', '/updates')
  })

  it('works on restricted devices without the "will not run" warning', async () => {
    setup({ devices: [device('a', { restricted: true, client_version: '0.1.0' }), device('b'), device('c')] })
    await version()
    expect(screen.queryByText(/restricted/)).not.toBeInTheDocument()
  })
})

describe('TargetSummary', () => {
  it('names a group, counts its members now and says membership is taken at creation', () => {
    render(<TargetSummary spec={{ type: 'group', group_id: 'g1' }} devices={devices} groups={groups} />)
    expect(screen.getByText('Group “Lab” (3 devices now)')).toBeInTheDocument()
    expect(screen.getByText(/Membership is taken at the moment the job is created/)).toBeInTheDocument()
    expect(screen.getByText(/1 of them is offline/)).toBeInTheDocument()
  })

  it('says when the group is gone', () => {
    render(<TargetSummary spec={{ type: 'group', group_id: 'nope' }} devices={devices} groups={groups} />)
    expect(screen.getByText('This group no longer exists')).toBeInTheDocument()
  })

  it('notes an empty group', () => {
    render(
      <TargetSummary
        spec={{ type: 'group', group_id: 'g1' }}
        devices={devices}
        groups={[{ ...groups[0]!, device_ids: [] }]}
      />,
    )
    expect(screen.getByText('Group “Lab” (0 devices now)')).toBeInTheDocument()
    expect(screen.getByText(/The group has no devices at the moment/)).toBeInTheDocument()
  })
})
