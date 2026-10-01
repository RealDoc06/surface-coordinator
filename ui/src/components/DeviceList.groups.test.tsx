import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'
import { describe, expect, it, vi } from 'vitest'
import type { ActionKind } from '../lib/actions'
import type { Device, Group, TargetSpec } from '../api/types'
import { device } from '../test/helpers'
import { DeviceList } from './DeviceList'
import type { MembershipOp } from './GroupMembershipDialog'

const devices: Device[] = [
  device('a1', { hostname: 'LAB-01', group_ids: ['g-lab'], client_version: '1.2.9', update_available: true }),
  device('a2', { hostname: 'LAB-02', group_ids: ['g-lab', 'g-front'], client_version: '1.2.10' }),
  device('b1', { hostname: 'OFFICE-01', group_ids: [], client_version: '0.1.0', restricted: true, update_available: true }),
  device('b2', { hostname: 'OFFICE-02' }), // from a coordinator without groups
]

const groups: Group[] = [
  { id: 'g-front', name: 'Front desk', created_at: 1, device_ids: ['a2'] },
  { id: 'g-lab', name: 'Lab', created_at: 1, device_ids: ['a1', 'a2'] },
]

function setup(props: Partial<Parameters<typeof DeviceList>[0]> = {}) {
  const spies = {
    onRun: vi.fn<(t: TargetSpec) => void>(),
    onMove: vi.fn<(t: TargetSpec) => void>(),
    onAction: vi.fn<(k: ActionKind, t: TargetSpec) => void>(),
    onLibrary: vi.fn<(t: TargetSpec) => void>(),
    onRunGroup: vi.fn<(id: string) => void>(),
    onGroupMembers: vi.fn<(op: MembershipOp, groupId: string, ids: string[]) => Promise<void>>(async () => {}),
  }
  const user = userEvent.setup()
  render(
    <MemoryRouter>
      <DeviceList devices={devices} keyLabels={{}} now={10_000} groups={groups} {...spies} {...props} />
    </MemoryRouter>,
  )
  return { ...spies, user }
}

const rowOf = (name: string) => screen.getByRole('row', { name: new RegExp(name) })
const groupFilter = () => screen.getByRole('combobox', { name: 'Group' })
const shownHostnames = () =>
  screen
    .getAllByRole('row')
    .slice(1)
    .map((row) => within(row).queryByRole('link')?.textContent)

describe('groups in the device list', () => {
  it('shows each device’s groups as chips, in group order, and a dash for none', () => {
    setup()
    expect(within(rowOf('LAB-01')).getByText('Lab')).toBeInTheDocument()
    const lab2 = rowOf('LAB-02')
    expect(within(lab2).getAllByText(/^(Lab|Front desk)$/).map((e) => e.textContent)).toEqual(['Front desk', 'Lab'])
    // The Groups column follows the Network column.
    const groupCell = (name: string) => within(rowOf(name)).getAllByRole('cell')[5]
    expect(groupCell('OFFICE-01')).toHaveTextContent('—')
    expect(groupCell('OFFICE-02')).toHaveTextContent('—')
  })

  it('has no groups column or filter when there are no groups', () => {
    setup({ groups: [] })
    expect(screen.queryByRole('columnheader', { name: 'Groups' })).not.toBeInTheDocument()
    expect(screen.queryByRole('combobox', { name: 'Group' })).not.toBeInTheDocument()
  })

  it('filters by group and by "not in a group"', async () => {
    const { user } = setup()
    await user.selectOptions(groupFilter(), 'Lab')
    expect(shownHostnames()).toEqual(['LAB-01', 'LAB-02'])
    await user.selectOptions(groupFilter(), 'Front desk')
    expect(shownHostnames()).toEqual(['LAB-02'])
    await user.selectOptions(groupFilter(), 'Not in a group')
    expect(shownHostnames()).toEqual(['OFFICE-01', 'OFFICE-02'])
    await user.selectOptions(groupFilter(), 'All groups')
    expect(shownHostnames()).toHaveLength(4)
  })

  it('combines the group filter with the others', async () => {
    const { user } = setup()
    await user.selectOptions(groupFilter(), 'Lab')
    await user.type(screen.getByRole('searchbox'), 'lab-02')
    expect(shownHostnames()).toEqual(['LAB-02'])
  })

  it('offers "Run on this group" only while a group is filtered', async () => {
    const { user, onRunGroup } = setup()
    expect(screen.queryByRole('button', { name: /Run on this group/ })).not.toBeInTheDocument()
    await user.selectOptions(groupFilter(), 'Not in a group')
    expect(screen.queryByRole('button', { name: /Run on this group/ })).not.toBeInTheDocument()
    await user.selectOptions(groupFilter(), 'Lab')
    await user.click(screen.getByRole('button', { name: /Run on this group/ }))
    expect(onRunGroup).toHaveBeenCalledExactlyOnceWith('g-lab')
  })
})

describe('client versions', () => {
  it('marks devices with an update available and restricted ones', () => {
    setup()
    expect(within(rowOf('LAB-01')).getByText('update available')).toBeInTheDocument()
    expect(within(rowOf('LAB-02')).queryByText('update available')).not.toBeInTheDocument()
    const old = rowOf('OFFICE-01')
    expect(within(old).getByText('restricted')).toHaveAttribute('title', expect.stringMatching(/too old for this coordinator.*only accepts client updates/))
    expect(within(old).getByText('0.1.0')).toBeInTheDocument()
  })

  it('shows plain versions for a coordinator that does not send the flags', () => {
    setup()
    const row = rowOf('OFFICE-02')
    expect(within(row).getByText('0.2.0')).toBeInTheDocument()
    expect(within(row).queryByText('restricted')).not.toBeInTheDocument()
  })
})

describe('more actions', () => {
  async function open(user: ReturnType<typeof userEvent.setup>) {
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    return screen.getByRole('menu')
  }

  it('hands the selection to the chosen action', async () => {
    const { user, onAction, onLibrary } = setup()
    await user.click(screen.getByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('checkbox', { name: 'Select OFFICE-01' }))
    const selection = { type: 'devices', device_ids: ['a1', 'b1'] }

    await user.click(within(await open(user)).getByRole('menuitem', { name: 'Launch app…' }))
    expect(onAction).toHaveBeenLastCalledWith('app.launch', selection)

    await user.click(within(await open(user)).getByRole('menuitem', { name: 'Apply setting…' }))
    expect(onAction).toHaveBeenLastCalledWith('setting.apply', selection)

    await user.click(within(await open(user)).getByRole('menuitem', { name: /Restart, shut down or sleep/ }))
    expect(onAction).toHaveBeenLastCalledWith('power', selection)

    await user.click(within(await open(user)).getByRole('menuitem', { name: 'Update client…' }))
    expect(onAction).toHaveBeenLastCalledWith('client.update', selection)

    await user.click(within(await open(user)).getByRole('menuitem', { name: 'Run script from library…' }))
    expect(onLibrary).toHaveBeenCalledExactlyOnceWith(selection)
  })

  it('disables the actions until something is selected', async () => {
    const { user } = setup()
    const menu = await open(user)
    for (const item of within(menu).getAllByRole('menuitem')) expect(item).toBeDisabled()
  })

  it('is operated with the keyboard', async () => {
    const { user, onAction } = setup()
    await user.click(screen.getByRole('checkbox', { name: 'Select LAB-01' }))
    const button = screen.getByRole('button', { name: /More actions/ })
    button.focus()
    await user.keyboard('{ArrowDown}')
    expect(screen.getByRole('menuitem', { name: 'Launch app…' })).toHaveFocus()
    await user.keyboard('{ArrowDown}{ArrowDown}')
    expect(screen.getByRole('menuitem', { name: /Restart, shut down/ })).toHaveFocus()
    await user.keyboard('{End}')
    expect(screen.getByRole('menuitem', { name: 'Remove from group…' })).toHaveFocus()
    await user.keyboard('{Home}{Enter}')
    expect(onAction).toHaveBeenCalledExactlyOnceWith('app.launch', { type: 'devices', device_ids: ['a1'] })
    expect(screen.queryByRole('menu')).not.toBeInTheDocument()
    expect(button).toHaveFocus()
  })

  it('closes with Escape and returns focus to the button', async () => {
    const { user } = setup()
    const button = screen.getByRole('button', { name: /More actions/ })
    await user.click(button)
    expect(button).toHaveAttribute('aria-expanded', 'true')
    await user.keyboard('{Escape}')
    expect(screen.queryByRole('menu')).not.toBeInTheDocument()
    expect(button).toHaveFocus()
  })

  it('closes when the user clicks elsewhere', async () => {
    const { user } = setup()
    await open(user)
    await user.click(screen.getByRole('searchbox'))
    expect(screen.queryByRole('menu')).not.toBeInTheDocument()
  })
})

describe('adding to and removing from groups', () => {
  it('adds the selection to the chosen group and says how many are new', async () => {
    const { user, onGroupMembers } = setup()
    await user.click(screen.getByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('checkbox', { name: 'Select OFFICE-01' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Add to group…' }))

    const dialog = screen.getByRole('dialog', { name: 'Add to group' })
    await user.selectOptions(within(dialog).getByRole('combobox', { name: 'Group' }), 'Lab (2 members)')
    // LAB-01 is already in Lab; only OFFICE-01 is new.
    expect(within(dialog).getByText(/1 device will be added to “Lab”/)).toBeInTheDocument()
    expect(within(dialog).getByText(/The others are already in it/)).toBeInTheDocument()
    await user.click(within(dialog).getByRole('button', { name: 'Add' }))

    expect(onGroupMembers).toHaveBeenCalledExactlyOnceWith('add', 'g-lab', ['a1', 'b1'])
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
  })

  it('only offers groups that contain a selected device when removing', async () => {
    const { user, onGroupMembers } = setup()
    await user.click(screen.getByRole('checkbox', { name: 'Select LAB-02' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Remove from group…' }))

    const dialog = screen.getByRole('dialog', { name: 'Remove from group' })
    const select = within(dialog).getByRole('combobox', { name: 'Group' })
    expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual([
      'Front desk (1 member)',
      'Lab (2 members)',
    ])
    await user.selectOptions(select, 'Lab (2 members)')
    expect(within(dialog).getByText(/1 device will be removed from “Lab”/)).toBeInTheDocument()
    await user.click(within(dialog).getByRole('button', { name: 'Remove' }))
    expect(onGroupMembers).toHaveBeenCalledExactlyOnceWith('remove', 'g-lab', ['a2'])
  })

  it('says so when none of the selection is in a group', async () => {
    const { user } = setup()
    await user.click(screen.getByRole('checkbox', { name: 'Select OFFICE-02' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Remove from group…' }))
    expect(screen.getByText('None of the selected devices is in a group.')).toBeInTheDocument()
  })

  it('keeps the dialog open and shows the reason when the change is refused', async () => {
    const { user } = setup({
      onGroupMembers: async () => {
        throw new Error('The coordinator refused.')
      },
    })
    await user.click(screen.getByRole('checkbox', { name: 'Select OFFICE-01' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: 'Add to group…' }))
    await user.click(screen.getByRole('button', { name: 'Add' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('The coordinator refused.')
    expect(screen.getByRole('dialog')).toBeInTheDocument()
  })
})
