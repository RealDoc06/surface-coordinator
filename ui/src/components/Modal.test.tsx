import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expect, it, vi } from 'vitest'
import { ConfirmDialog } from './Modal'

it('keeps an in-flight confirmation visible instead of implying that dismissal cancels the operation', async () => {
  const user = userEvent.setup()
  const onCancel = vi.fn()
  const onConfirm = vi.fn()
  const view = render(
    <ConfirmDialog title="Move devices" confirmLabel="Moving…" busy onCancel={onCancel} onConfirm={onConfirm}>
      Connecting to the new network.
    </ConfirmDialog>,
  )
  expect(screen.getByRole('button', { name: 'Cancel' })).toBeDisabled()
  expect(screen.getByRole('button', { name: 'Moving…' })).toBeDisabled()
  await user.keyboard('{Escape}{Tab}')
  expect(onCancel).not.toHaveBeenCalled()
  expect(screen.getByRole('dialog')).toHaveFocus()
  view.rerender(
    <ConfirmDialog title="Move devices" confirmLabel="Try again" onCancel={onCancel} onConfirm={onConfirm}>
      The operation did not complete.
    </ConfirmDialog>,
  )
  await user.keyboard('{Escape}')
  expect(onCancel).toHaveBeenCalledOnce()
})
