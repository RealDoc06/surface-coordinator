import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import { describe, expect, it, vi } from 'vitest'
import type { Device, TargetSpec } from '../api/types'
import { device } from '../test/helpers'
import { DeviceList } from './DeviceList'

const NOW = 10 * 60_000

const devices: Device[] = [
  device('a1', { hostname: 'LAB-01', online: true }),
  device('a2', { hostname: 'LAB-02', online: false, last_seen_at: NOW - 3 * 60_000 }),
  device('b1', { hostname: 'OFFICE-01', online: true, enrollment_key_id: 'key-unknown-0123456789' }),
  device('b2', { hostname: 'OFFICE-02', online: false, last_seen_at: NOW - 2 * 3_600_000 }),
]

function setup(props: Partial<Parameters<typeof DeviceList>[0]> = {}) {
  const onRun = vi.fn<(targets: TargetSpec) => void>()
  const user = userEvent.setup()
  render(
    <MemoryRouter>
      <DeviceList devices={devices} keyLabels={{ 'key-1': 'Classroom' }} now={NOW} onRun={onRun} {...props} />
    </MemoryRouter>,
  )
  return { onRun, user }
}

const checkbox = (hostname: string) => screen.getByRole('checkbox', { name: `Select ${hostname}` })
const selectAll = () => screen.getByRole<HTMLInputElement>('checkbox', { name: 'Select all shown devices' })
const runButton = () => screen.getByRole('button', { name: /^Run script/ })
const rowNames = () =>
  screen
    .getAllByRole('row')
    .slice(1)
    .map((row) => within(row).queryByRole('link')?.textContent)

describe('DeviceList', () => {
  it('lists devices with status, last seen and key label', () => {
    setup()
    const row = screen.getByRole('row', { name: /LAB-02/ })
    expect(within(row).getByRole('img', { name: 'Offline' })).toBeInTheDocument()
    expect(within(row).getByText('3 min ago')).toBeInTheDocument()
    expect(within(row).getByText('Classroom')).toBeInTheDocument()

    const online = screen.getByRole('row', { name: /LAB-01/ })
    expect(within(online).getByRole('img', { name: 'Online' })).toBeInTheDocument()
    expect(within(online).getByText('now')).toBeInTheDocument()

    expect(screen.getByRole('link', { name: 'OFFICE-01' })).toHaveAttribute('href', '/devices/b1')
  })

  it('starts with nothing selected and the run action disabled', () => {
    setup()
    expect(screen.getByText('0 selected')).toBeInTheDocument()
    expect(runButton()).toBeDisabled()
  })

  it('selects individual devices and runs on exactly those', async () => {
    const { onRun, user } = setup()
    await user.click(checkbox('LAB-02'))
    await user.click(checkbox('OFFICE-01'))
    expect(screen.getByText('2 selected')).toBeInTheDocument()

    await user.click(checkbox('LAB-02'))
    expect(screen.getByText('1 selected')).toBeInTheDocument()

    await user.click(runButton())
    expect(onRun).toHaveBeenCalledExactlyOnceWith({ type: 'devices', device_ids: ['b1'] })
  })

  it('filters by hostname or id and by online state', async () => {
    const { user } = setup()
    await user.type(screen.getByRole('searchbox'), 'office')
    expect(rowNames()).toEqual(['OFFICE-01', 'OFFICE-02'])

    await user.selectOptions(screen.getByRole('combobox', { name: 'Show' }), 'offline')
    expect(rowNames()).toEqual(['OFFICE-02'])

    await user.clear(screen.getByRole('searchbox'))
    await user.type(screen.getByRole('searchbox'), 'A1')
    await user.selectOptions(screen.getByRole('combobox', { name: 'Show' }), 'all')
    expect(rowNames()).toEqual(['LAB-01'])
  })

  it('select-all applies to the filtered rows only', async () => {
    const { onRun, user } = setup()
    await user.type(screen.getByRole('searchbox'), 'lab')
    await user.click(selectAll())
    expect(screen.getByText('2 selected')).toBeInTheDocument()
    expect(selectAll().checked).toBe(true)

    // The selection survives a change of filter, and says what is hidden.
    await user.clear(screen.getByRole('searchbox'))
    await user.type(screen.getByRole('searchbox'), 'office')
    expect(screen.getByText('2 selected')).toBeInTheDocument()
    expect(screen.getByText('(2 not shown)')).toBeInTheDocument()
    expect(selectAll().checked).toBe(false)

    await user.click(checkbox('OFFICE-02'))
    expect(selectAll().indeterminate).toBe(true)

    await user.click(runButton())
    expect(onRun).toHaveBeenCalledExactlyOnceWith({ type: 'devices', device_ids: ['a1', 'a2', 'b2'] })
  })

  it('select-all toggles off only the filtered rows', async () => {
    const { user } = setup({ initialSelected: ['a1', 'a2', 'b1', 'b2'] })
    expect(screen.getByText('4 selected')).toBeInTheDocument()
    expect(selectAll().checked).toBe(true)

    await user.selectOptions(screen.getByRole('combobox', { name: 'Show' }), 'offline')
    await user.click(selectAll())
    expect(screen.getByText('2 selected')).toBeInTheDocument()

    await user.click(screen.getByRole('button', { name: 'Clear' }))
    expect(screen.getByText('0 selected')).toBeInTheDocument()
  })

  it('offers to run on all devices regardless of the selection', async () => {
    const { onRun, user } = setup()
    await user.click(screen.getByRole('button', { name: 'Run on all devices' }))
    expect(onRun).toHaveBeenCalledExactlyOnceWith({ type: 'all' })
  })

  it('does not count selected devices that no longer exist', () => {
    setup({ initialSelected: ['a1', 'gone'] })
    expect(screen.getByText('1 selected')).toBeInTheDocument()
  })

  it('explains how to enroll a device when there are none', () => {
    setup({ devices: [] })
    expect(screen.getByRole('heading', { name: 'No devices yet' })).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Enrollment keys' })).toHaveAttribute('href', '/keys')
    expect(screen.getByText(/--key-id/)).toBeInTheDocument()
    expect(screen.queryByRole('table')).not.toBeInTheDocument()
  })
})
