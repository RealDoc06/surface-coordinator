import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { App } from '../App'
import { api } from '../api/client'
import { baseSession, callsTo, fakeCoordinator, group } from '../test/coordinator'
import { FakeWebSocket } from '../test/helpers'

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

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (reason: unknown) => void
  const promise = new Promise<T>((done, fail) => {
    resolve = done
    reject = fail
  })
  return { promise, resolve, reject }
}

async function expectPendingDialog(user: ReturnType<typeof userEvent.setup>, dialog: HTMLElement, field: HTMLElement) {
  expect(dialog).toHaveAttribute('aria-busy', 'true')
  expect(field).toBeDisabled()
  const cancel = within(dialog).getByRole('button', { name: 'Cancel' })
  expect(cancel).toBeDisabled()
  await user.click(cancel)
  await user.keyboard('{Escape}')
  fireEvent.mouseDown(dialog.parentElement!)
  expect(dialog).toBeInTheDocument()
}

describe('pending modal writes', () => {
  it('keeps a password change open until it finishes, then allows retry after failure', async () => {
    fakeCoordinator({
      session: {
        ...baseSession,
        auth_mode: 'accounts',
        account: { id: 'admin', username: 'admin' },
        capabilities: { accounts: true, hotspot: false },
      },
      override({ method, path }) {
        if (method === 'GET' && path === '/accounts') {
          return { body: { accounts: [{ id: 'admin', username: 'admin', created_at: 1000 }] } }
        }
        if (method === 'POST' && path === '/accounts/admin/password') return { status: 204 }
      },
    })
    const pending = deferred<Awaited<ReturnType<typeof api.setAccountPassword>>>()
    const write = vi.spyOn(api, 'setAccountPassword').mockImplementationOnce(() => pending.promise)
    const user = start('/accounts')
    await user.click(await screen.findByRole('button', { name: 'Change password of admin' }))
    const dialog = screen.getByRole('dialog')
    const password = within(dialog).getByLabelText('New password (at least 10 characters)')
    await user.type(password, 'replacement-password')
    await user.click(within(dialog).getByRole('button', { name: 'Change password' }))
    await expectPendingDialog(user, dialog, password)
    expect(write).toHaveBeenCalledExactlyOnceWith('admin', 'replacement-password')

    await act(async () => pending.reject(new Error('Password change failed')))
    expect(within(dialog).getByRole('alert')).toHaveTextContent('Password change failed')
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toBeEnabled()
    expect(password).toBeEnabled()
    await user.click(within(dialog).getByRole('button', { name: 'Change password' }))
    await waitFor(() => expect(dialog).not.toBeInTheDocument())
    expect(write).toHaveBeenCalledTimes(2)
    expect(screen.getByRole('status')).toHaveTextContent('Password of admin changed.')
  })

  it('keeps a group rename open and shows a delayed error without losing the entered name', async () => {
    const server = fakeCoordinator({ groups: [group('g1', 'Before')] })
    const pending = deferred<Awaited<ReturnType<typeof api.renameGroup>>>()
    const write = vi.spyOn(api, 'renameGroup').mockImplementationOnce(() => pending.promise)
    const user = start('/groups')
    await user.click(await screen.findByRole('button', { name: 'Rename Before' }))
    const dialog = screen.getByRole('dialog')
    const name = within(dialog).getByRole('textbox', { name: 'Name' })
    await user.clear(name)
    await user.type(name, 'After')
    await user.click(within(dialog).getByRole('button', { name: 'Rename' }))
    await expectPendingDialog(user, dialog, name)
    expect(write).toHaveBeenCalledExactlyOnceWith('g1', 'After')

    await act(async () => pending.reject(new Error('Rename failed')))
    expect(within(dialog).getByRole('alert')).toHaveTextContent('Rename failed')
    expect(name).toHaveValue('After')
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toBeEnabled()
    await user.click(within(dialog).getByRole('button', { name: 'Rename' }))
    await screen.findByRole('link', { name: 'After' })
    expect(dialog).not.toBeInTheDocument()
    expect(server.groups[0]?.name).toBe('After')
  })

  it.each([
    { action: 'Add', menu: 'Add to group…', members: ['b'], expected: ['b', 'a'] },
    { action: 'Remove', menu: 'Remove from group…', members: ['a', 'b'], expected: ['b'] },
  ])('keeps $action membership changes open and retryable after a delayed failure', async ({ action, menu, members, expected }) => {
    const server = fakeCoordinator({ groups: [group('g1', 'Lab', members)] })
    const pending = deferred<Awaited<ReturnType<typeof api.setGroupMembers>>>()
    const write = vi.spyOn(api, 'setGroupMembers').mockImplementationOnce(() => pending.promise)
    const user = start()
    await user.click(await screen.findByRole('checkbox', { name: 'Select LAB-01' }))
    await user.click(screen.getByRole('button', { name: /More actions/ }))
    await user.click(screen.getByRole('menuitem', { name: menu }))
    const dialog = screen.getByRole('dialog')
    await user.click(within(dialog).getByRole('button', { name: action }))
    await waitFor(() => expect(write).toHaveBeenCalledExactlyOnceWith('g1', expected))
    await expectPendingDialog(user, dialog, within(dialog).getByRole('combobox', { name: 'Group' }))

    await act(async () => pending.reject(new Error('Membership change failed')))
    expect(within(dialog).getByRole('alert')).toHaveTextContent('Membership change failed')
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toBeEnabled()
    await user.click(within(dialog).getByRole('button', { name: action }))
    await waitFor(() => expect(dialog).not.toBeInTheDocument())
    expect(server.groups[0]?.device_ids).toEqual(expected)
    expect(callsTo(server.calls, 'PUT', '/groups/g1/members')[0]?.body).toEqual({ device_ids: expected })
  })

  it('keeps a library save open so a delayed error can be corrected and retried', async () => {
    const server = fakeCoordinator()
    const pending = deferred<Awaited<ReturnType<typeof api.createScript>>>()
    const write = vi.spyOn(api, 'createScript').mockImplementationOnce(() => pending.promise)
    const user = start('/run')
    await user.type(await screen.findByRole('textbox', { name: 'Script body' }), 'Write-Output hello')
    await user.click(screen.getByRole('button', { name: 'Save to library…' }))
    const dialog = screen.getByRole('dialog')
    const name = within(dialog).getByRole('textbox', { name: 'Name' })
    await user.type(name, 'Hello')
    await user.click(within(dialog).getByRole('button', { name: 'Save' }))
    await expectPendingDialog(user, dialog, name)
    expect(write).toHaveBeenCalledExactlyOnceWith({ name: 'Hello', shell: 'powershell', body: 'Write-Output hello' })

    await act(async () => pending.reject(new Error('Save failed')))
    expect(within(dialog).getByRole('alert')).toHaveTextContent('Save failed')
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toBeEnabled()
    await user.clear(name)
    await user.type(name, 'Hello again')
    await user.click(within(dialog).getByRole('button', { name: 'Save' }))
    await waitFor(() => expect(dialog).not.toBeInTheDocument())
    expect(screen.getByRole('status')).toHaveTextContent('Saved to the library as “Hello again”.')
    expect(server.scripts).toHaveLength(1)
    expect(server.scripts[0]?.name).toBe('Hello again')
  })
})
