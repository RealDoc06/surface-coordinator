import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Device, Job, TargetSpec } from '../api/types'
import type { WifiPrefill } from '../lib/targets'
import { apiError, device, job, mockFetch, target, type Call } from '../test/helpers'
import { WifiJoinForm } from './WifiJoinForm'

const devices: Device[] = [
  device('a', { hostname: 'LAB-01' }),
  device('b', { hostname: 'LAB-02', online: false }),
  device('c', { hostname: 'LAB-03' }),
]

const created = job('job-9', [target('t1', 'a'), target('t2', 'b')], { kind: 'wifi.join' })

function setup(
  options: {
    targets?: TargetSpec | null
    initial?: WifiPrefill
    reply?: Parameters<typeof mockFetch>[0]
  } = {},
) {
  const calls = mockFetch(options.reply ?? (() => ({ status: 201, body: { job: created } })))
  const onCreated = vi.fn<(job: Job) => void>()
  const user = userEvent.setup()
  render(
    <WifiJoinForm
      targets={options.targets === undefined ? { type: 'devices', device_ids: ['a', 'b'] } : options.targets}
      devices={devices}
      initial={options.initial}
      onCreated={onCreated}
    />,
  )
  return { calls, onCreated, user }
}

const ssidField = () => screen.getByRole('textbox', { name: 'Network name (SSID)' })
const passwordField = () => screen.getByLabelText('Password')
const urlField = () => screen.getByRole('textbox', { name: /Coordinator address/ })
const submit = () => screen.getByRole('button', { name: /^Move/ })
const jobPosts = (calls: Call[]) => calls.filter((c) => c.method === 'POST' && c.path === '/jobs')

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('WifiJoinForm', () => {
  it('warns that devices leave their network and what happens with a local coordinator', () => {
    setup()
    const warning = screen.getByRole('note')
    expect(warning).toHaveTextContent(/These devices will leave the network they are on now/)
    expect(warning).toHaveTextContent(/Destination unconfirmed.*after two minutes/)
  })

  it('summarises the targets and counts them on the button', () => {
    setup()
    expect(screen.getByText('2 selected devices: LAB-01, LAB-02')).toBeInTheDocument()
    expect(submit()).toHaveTextContent('Move 2 devices to this network')
  })

  it('sends a wifi.join job with only what was filled in', async () => {
    const { calls, onCreated, user } = setup()
    await user.type(ssidField(), 'Classroom')
    await user.click(submit())

    await waitFor(() => expect(onCreated).toHaveBeenCalledWith(created))
    expect(jobPosts(calls).map((c) => c.body)).toEqual([
      { kind: 'wifi.join', payload: { ssid: 'Classroom' }, targets: { type: 'devices', device_ids: ['a', 'b'] } },
    ])
  })

  it('sends the password and coordinator address when given', async () => {
    const { calls, user } = setup({ targets: { type: 'all' } })
    await user.type(ssidField(), 'Lab net')
    await user.type(passwordField(), 'correct horse')
    await user.type(urlField(), 'ws://192.168.137.1:8081')
    await user.click(submit())

    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toEqual({
      kind: 'wifi.join',
      payload: { ssid: 'Lab net', password: 'correct horse', coordinator_url: 'ws://192.168.137.1:8081' },
      targets: { type: 'all' },
    })
  })

  it('hides the password until asked to show it', async () => {
    const { user } = setup()
    expect(passwordField()).toHaveAttribute('type', 'password')
    await user.click(screen.getByRole('button', { name: 'Show' }))
    expect(passwordField()).toHaveAttribute('type', 'text')
    await user.click(screen.getByRole('button', { name: 'Hide' }))
    expect(passwordField()).toHaveAttribute('type', 'password')
  })

  it('requires an SSID and sends nothing', async () => {
    const { calls, user } = setup()
    await user.click(submit())
    expect(await screen.findByText(/Enter the name \(SSID\)/)).toBeInTheDocument()
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('validates the SSID in bytes: 16 two-byte characters fit', async () => {
    const { calls, user } = setup()
    await user.type(ssidField(), 'é'.repeat(16))
    expect(screen.getByText(/32 bytes \(32 used\)/)).toBeInTheDocument()
    await user.click(submit())
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ payload: { ssid: 'é'.repeat(16) } })
  })

  it('validates the SSID in bytes: 17 two-byte characters are too long', async () => {
    const { calls, user } = setup()
    await user.type(ssidField(), 'é'.repeat(17))
    expect(screen.getByText(/32 bytes \(34 used\)/)).toBeInTheDocument()
    await user.click(submit())
    expect(await screen.findByText(/at most 32 bytes; this one is 34/)).toBeInTheDocument()
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('rejects passwords of the wrong length but accepts none', async () => {
    const { calls, user } = setup()
    await user.type(ssidField(), 'Guest')
    await user.type(passwordField(), 'short')
    await user.click(submit())
    expect(await screen.findByText(/8 to 63 characters; this one is 5/)).toBeInTheDocument()
    expect(jobPosts(calls)).toHaveLength(0)

    await user.clear(passwordField())
    await user.click(submit())
    await waitFor(() => expect(jobPosts(calls)).toHaveLength(1))
    expect(jobPosts(calls)[0]?.body).toMatchObject({ payload: { ssid: 'Guest' } })
  })

  it('rejects a coordinator address that is not ws:// or wss://', async () => {
    const { calls, user } = setup()
    await user.type(ssidField(), 'Guest')
    await user.type(urlField(), 'http://192.168.0.2:8081')
    await user.click(submit())
    expect(await screen.findByRole('alert')).toHaveTextContent(/start with ws:\/\/ or wss:\/\//)
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('explains what the coordinator address means', () => {
    setup()
    expect(screen.getByText(/Devices will use this coordinator from then on/)).toBeInTheDocument()
  })

  it('needs at least one device', async () => {
    const { calls, user } = setup({ targets: { type: 'devices', device_ids: [] } })
    await user.type(ssidField(), 'Guest')
    await user.click(submit())
    expect(await screen.findByText('Select at least one device to move.')).toBeInTheDocument()
    expect(jobPosts(calls)).toHaveLength(0)
  })

  it('shows the coordinator complaint and lets the user try again', async () => {
    let attempts = 0
    const { onCreated, user } = setup({
      reply: () =>
        attempts++ === 0
          ? apiError(400, 'invalid_payload', 'ssid is too long')
          : { status: 201, body: { job: created } },
    })
    await user.type(ssidField(), 'Guest')
    await user.click(submit())
    expect(await screen.findByText('ssid is too long')).toBeInTheDocument()
    expect(onCreated).not.toHaveBeenCalled()
    await user.click(submit())
    await waitFor(() => expect(onCreated).toHaveBeenCalled())
  })

  it('starts from a previous job but never knows its password', () => {
    setup({ initial: { ssid: 'Classroom', coordinator_url: 'ws://10.0.0.1:8081', passwordNeeded: true } })
    expect(ssidField()).toHaveValue('Classroom')
    expect(urlField()).toHaveValue('ws://10.0.0.1:8081')
    expect(passwordField()).toHaveValue('')
    expect(screen.getByText(/enter it again/)).toBeInTheDocument()
  })
})
