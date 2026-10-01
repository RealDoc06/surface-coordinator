import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Device, Job, ScriptRun, TargetSpec } from '../api/types'
import { apiError, device, job, mockFetch, target, type Call } from '../test/helpers'
import { parseTimeout, ScriptComposer } from './ScriptComposer'

const devices: Device[] = [
  device('a', { hostname: 'LAB-01' }),
  device('b', { hostname: 'LAB-02', online: false }),
  device('c', { hostname: 'LAB-03' }),
]

const created = job('job-9', [target('t1', 'a'), target('t2', 'b')])

function setup(
  options: {
    targets?: TargetSpec | null
    initial?: Partial<ScriptRun>
    reply?: Parameters<typeof mockFetch>[0]
    devices?: Device[]
  } = {},
) {
  const calls = mockFetch(options.reply ?? (() => ({ status: 201, body: { job: created } })))
  const onCreated = vi.fn<(job: Job) => void>()
  const user = userEvent.setup()
  render(
    <ScriptComposer
      targets={options.targets === undefined ? { type: 'devices', device_ids: ['a', 'b'] } : options.targets}
      devices={options.devices ?? devices}
      initial={options.initial}
      onCreated={onCreated}
    />,
  )
  return { calls, onCreated, user }
}

const bodyField = () => screen.getByRole('textbox', { name: 'Script body' })
const timeoutField = () => screen.getByRole('textbox', { name: 'Timeout (seconds)' })
const submit = () => screen.getByRole('button', { name: /^Run/ })
const jobPosts = (calls: Call[]) => calls.filter((c) => c.method === 'POST' && c.path === '/jobs')

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('ScriptComposer', () => {
  it('starts with PowerShell, system and a 300 second timeout', () => {
    setup()
    expect(screen.getByRole('radio', { name: 'PowerShell' })).toBeChecked()
    expect(screen.getByRole('radio', { name: 'System' })).toBeChecked()
    expect(timeoutField()).toHaveValue('300')
    expect(bodyField()).toHaveValue('')
  })

  it('summarises selected devices and says what happens to offline ones', () => {
    setup()
    expect(screen.getByText('2 selected devices: LAB-01, LAB-02')).toBeInTheDocument()
    expect(screen.getByText(/1 of them is offline\. Offline devices get the job when they reconnect\./)).toBeInTheDocument()
    expect(submit()).toHaveTextContent('Run on 2 devices')
  })

  it('summarises all devices with their count', () => {
    setup({ targets: { type: 'all' } })
    expect(screen.getByText('All devices (3)')).toBeInTheDocument()
    expect(screen.getByText(/1 of them is offline/)).toBeInTheDocument()
  })

  it('creates the job with the entered values and reports it', async () => {
    const { calls, onCreated, user } = setup()
    await user.click(screen.getByRole('radio', { name: 'cmd' }))
    await user.click(screen.getByRole('radio', { name: 'Logged-in user' }))
    await user.clear(timeoutField())
    await user.type(timeoutField(), '45')
    await user.type(bodyField(), 'echo hello{Enter}exit /b 3')
    await user.click(submit())

    await waitFor(() => expect(onCreated).toHaveBeenCalledExactlyOnceWith(created))
    expect(jobPosts(calls)).toEqual([
      {
        method: 'POST',
        path: '/jobs',
        body: {
          kind: 'script.run',
          payload: { shell: 'cmd', body: 'echo hello\nexit /b 3', timeout_secs: 45, run_as: 'user' },
          targets: { type: 'devices', device_ids: ['a', 'b'] },
        },
      },
    ])
  })

  it('targets all devices when asked to', async () => {
    const { calls, onCreated, user } = setup({ targets: { type: 'all' } })
    await user.type(bodyField(), 'Get-Date')
    await user.click(submit())

    await waitFor(() => expect(onCreated).toHaveBeenCalled())
    expect(jobPosts(calls)[0]?.body).toEqual({
      kind: 'script.run',
      payload: { shell: 'powershell', body: 'Get-Date', timeout_secs: 300, run_as: 'system' },
      targets: { type: 'all' },
    })
  })

  it('opens pre-filled for "Run again"', () => {
    setup({ initial: { shell: 'cmd', body: 'dir C:\\', timeout_secs: 60, run_as: 'user' } })
    expect(screen.getByRole('radio', { name: 'cmd' })).toBeChecked()
    expect(screen.getByRole('radio', { name: 'Logged-in user' })).toBeChecked()
    expect(timeoutField()).toHaveValue('60')
    expect(bodyField()).toHaveValue('dir C:\\')
  })

  it('refuses an empty script', async () => {
    const { calls, onCreated, user } = setup()
    await user.type(bodyField(), '   ')
    await user.click(submit())

    expect(screen.getByText('Enter the script to run.')).toBeInTheDocument()
    expect(bodyField()).toBeInvalid()
    expect(jobPosts(calls)).toEqual([])
    expect(onCreated).not.toHaveBeenCalled()
  })

  it.each(['0', '86401', '', '12.5', 'abc', '-5'])('refuses the timeout "%s"', async (value) => {
    const { calls, user } = setup()
    await user.type(bodyField(), 'Get-Date')
    await user.clear(timeoutField())
    if (value !== '') await user.type(timeoutField(), value)
    await user.click(submit())

    expect(screen.getByText(/whole number of seconds from 1 to 86400/)).toBeInTheDocument()
    expect(jobPosts(calls)).toEqual([])
  })

  it('accepts the timeout limits', () => {
    expect(parseTimeout('1')).toBe(1)
    expect(parseTimeout(' 86400 ')).toBe(86400)
    expect(parseTimeout('86401')).toBeNull()
    expect(parseTimeout('0')).toBeNull()
  })

  it('refuses to run without a target', async () => {
    const { calls, user } = setup({ targets: null })
    await user.type(bodyField(), 'Get-Date')
    await user.click(submit())
    expect(screen.getByText('Select at least one device to run on.')).toBeInTheDocument()
    expect(jobPosts(calls)).toEqual([])
  })

  it('refuses an empty selection and "all" when there are no devices', async () => {
    const first = setup({ targets: { type: 'devices', device_ids: [] } })
    await first.user.type(bodyField(), 'Get-Date')
    await first.user.click(submit())
    expect(screen.getByText('Select at least one device to run on.')).toBeInTheDocument()
    expect(jobPosts(first.calls)).toEqual([])
  })

  it('refuses "all devices" when there are none', async () => {
    const { calls, user } = setup({ targets: { type: 'all' }, devices: [] })
    await user.type(bodyField(), 'Get-Date')
    await user.click(submit())
    expect(screen.getByText('There are no devices to run on yet.')).toBeInTheDocument()
    expect(jobPosts(calls)).toEqual([])
  })

  it('leaves out selected devices that no longer exist, and says so', async () => {
    const { calls, onCreated, user } = setup({ targets: { type: 'devices', device_ids: ['a', 'gone'] } })
    expect(screen.getByText('1 selected device: LAB-01')).toBeInTheDocument()
    expect(screen.getByText(/1 selected device no longer exists and will be skipped/)).toBeInTheDocument()

    await user.type(bodyField(), 'Get-Date')
    await user.click(submit())
    await waitFor(() => expect(onCreated).toHaveBeenCalled())
    expect(jobPosts(calls)[0]?.body).toMatchObject({ targets: { type: 'devices', device_ids: ['a'] } })
  })

  it('shows an API error inline and lets the user try again', async () => {
    let attempt = 0
    const { calls, onCreated, user } = setup({
      reply: () => {
        attempt += 1
        return attempt === 1
          ? apiError(400, 'invalid_payload', 'body must not be longer than 1 MiB')
          : { status: 201, body: { job: created } }
      },
    })
    await user.type(bodyField(), 'Get-Date')
    await user.click(submit())

    expect(await screen.findByRole('alert')).toHaveTextContent('body must not be longer than 1 MiB')
    expect(onCreated).not.toHaveBeenCalled()
    expect(bodyField()).toHaveValue('Get-Date')

    await user.click(submit())
    await waitFor(() => expect(onCreated).toHaveBeenCalledExactlyOnceWith(created))
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
    expect(jobPosts(calls)).toHaveLength(2)
  })

  it('explains known API error codes', async () => {
    const { user } = setup({ reply: () => apiError(400, 'unknown_device') })
    await user.type(bodyField(), 'Get-Date')
    await user.click(submit())
    expect(await screen.findByRole('alert')).toHaveTextContent(/no longer exists/)
  })

  it('reports an unreachable coordinator', async () => {
    const { user } = setup({
      reply: () => {
        throw new TypeError('Failed to fetch')
      },
    })
    await user.type(bodyField(), 'Get-Date')
    await user.click(submit())
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not reach the coordinator.')
  })

  it('submits with Ctrl+Enter from the script body', async () => {
    const { onCreated, user } = setup()
    await user.type(bodyField(), 'Get-Date{Control>}{Enter}{/Control}')
    await waitFor(() => expect(onCreated).toHaveBeenCalledExactlyOnceWith(created))
  })
})
