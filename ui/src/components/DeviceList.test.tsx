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
  const onMove = vi.fn<(targets: TargetSpec) => void>()
  const user = userEvent.setup()
  render(
    <MemoryRouter>
      <DeviceList devices={devices} keyLabels={{ 'key-1': 'Classroom' }} now={NOW} onRun={onRun} onMove={onMove} {...props} />
    </MemoryRouter>,
  )
  return { onRun, onMove, user }
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
    await user.click(screen.getByRole('button', { name: 'All devices' }))
    await user.click(screen.getByRole('menuitem', { name: 'Run on all devices' }))
    expect(onRun).toHaveBeenCalledExactlyOnceWith({ type: 'all' })
  })

  it('does not count selected devices that no longer exist', () => {
    setup({ initialSelected: ['a1', 'gone'] })
    expect(screen.getByText('1 selected')).toBeInTheDocument()
  })

  it('selects only online devices in the current filter', async () => {
    const { user, onRun } = setup({ initialSelected: ['b2'] })
    await user.type(screen.getByRole('searchbox'), 'lab')
    await user.click(screen.getByRole('button', { name: 'Select online' }))
    expect(screen.getByText('1 selected')).toBeInTheDocument()
    await user.click(runButton())
    expect(onRun).toHaveBeenCalledExactlyOnceWith({ type: 'devices', device_ids: ['a1'] })
  })

  it('searches by network and IP, and resets all filters without clearing selection', async () => {
    const { user } = setup({ devices: [
      device('a', { hostname: 'ROOM-01', network: { ssid: 'Classroom', ip: '10.0.0.21' } }),
      device('b', { hostname: 'ROOM-02', network: { ssid: 'Office', ip: '10.0.1.22' }, online: false }),
    ] })
    await user.type(screen.getByRole('searchbox'), 'classroom')
    expect(rowNames()).toEqual(['ROOM-01'])
    await user.click(checkbox('ROOM-01'))
    await user.clear(screen.getByRole('searchbox'))
    await user.type(screen.getByRole('searchbox'), '10.0.1.')
    expect(rowNames()).toEqual(['ROOM-02'])
    await user.selectOptions(screen.getByRole('combobox', { name: 'Show' }), 'offline')
    await user.click(screen.getByRole('button', { name: 'Reset filters' }))
    expect(rowNames()).toEqual(['ROOM-01', 'ROOM-02'])
    expect(screen.getByText('1 selected')).toBeInTheDocument()
  })

  it('does not claim a stale online device was seen now when live updates are disconnected', () => {
    setup({ devices: [device('a', { hostname: 'ROOM-01', last_seen_at: NOW - 3 * 60_000 })], live: false })
    expect(within(screen.getByRole('row', { name: /ROOM-01/ })).getByText('3 min ago')).toBeInTheDocument()
    expect(screen.queryByText('now')).not.toBeInTheDocument()
  })

  it('explains how to enroll a device when there are none', () => {
    setup({ devices: [] })
    expect(screen.getByRole('heading', { name: 'No devices yet' })).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Enrollment keys' })).toHaveAttribute('href', '/keys')
    expect(screen.getByText(/--key-id/)).toBeInTheDocument()
    expect(screen.queryByRole('table')).not.toBeInTheDocument()
  })

  describe('network column', () => {
    const networked: Device[] = [
      device('n1', { hostname: 'WIFI-01', network: { ssid: 'Classroom WiFi', ip: '192.168.4.20' } }),
      device('n2', { hostname: 'WIRED-01', network: { ssid: null, ip: '10.0.0.7' } }),
      device('n3', { hostname: 'SILENT-01', network: { ssid: null, ip: null } }),
      device('n4', { hostname: 'OLD-01' }),
    ]

    it('shows the SSID with the address beside it and as a tooltip', () => {
      setup({ devices: networked })
      expect(screen.getByRole('columnheader', { name: 'Network' })).toBeInTheDocument()
      const row = screen.getByRole('row', { name: /WIFI-01/ })
      expect(within(row).getByText('Classroom WiFi')).toBeInTheDocument()
      expect(within(row).getByText('192.168.4.20')).toBeInTheDocument()
      expect(within(row).getByTitle('Classroom WiFi (192.168.4.20)')).toBeInTheDocument()
    })

    it('shows just the address when there is no SSID', () => {
      setup({ devices: networked })
      const row = screen.getByRole('row', { name: /WIRED-01/ })
      expect(within(row).getByText('10.0.0.7')).toBeInTheDocument()
    })

    it('shows a dash when the device reported nothing, or the coordinator does not send it', () => {
      setup({ devices: networked })
      for (const name of [/SILENT-01/, /OLD-01/]) {
        const cells = within(screen.getByRole('row', { name })).getAllByRole('cell')
        // check, status, hostname, OS, network
        expect(cells[4]).toHaveTextContent('—')
      }
    })
  })

  describe('moving to Wi-Fi', () => {
    it('moves exactly the selected devices', async () => {
      const { onMove, user } = setup()
      expect(screen.getByRole('button', { name: /^Move to Wi-Fi/ })).toBeDisabled()
      await user.click(checkbox('LAB-01'))
      await user.click(checkbox('OFFICE-02'))
      await user.click(screen.getByRole('button', { name: /^Move to Wi-Fi/ }))
      expect(onMove).toHaveBeenCalledWith({ type: 'devices', device_ids: ['a1', 'b2'] })
    })

    it('moves all devices', async () => {
      const { onMove, user } = setup()
      await user.click(screen.getByRole('button', { name: 'All devices' }))
      await user.click(screen.getByRole('menuitem', { name: /^Move all to Wi-Fi/ }))
      expect(onMove).toHaveBeenCalledWith({ type: 'all' })
    })
  })
})

describe('DeviceList: moving to the manager\'s own Wi-Fi', () => {
  it('offers the network by name and passes the selection on', async () => {
    const onMoveLocal = vi.fn<(targets: TargetSpec) => void>()
    const { user } = setup({ localWifi: 'Office-Net', onMoveLocal })
    const button = screen.getByRole('button', { name: 'Move to Office-Net' })
    expect(button).toBeDisabled()

    await user.click(checkbox('LAB-01'))
    await user.click(button)
    expect(onMoveLocal).toHaveBeenCalledWith({ type: 'devices', device_ids: ['a1'] })
  })

  it('is absent when the manager has no network to offer', () => {
    setup()
    expect(screen.queryByRole('button', { name: /^Move to (?!Wi-Fi)/ })).toBeNull()
  })
})
