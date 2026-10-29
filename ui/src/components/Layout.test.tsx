import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter, useLocation, useNavigate } from 'react-router-dom'
import { expect, it, vi } from 'vitest'
import { initialState } from '../state/store'
import { baseSession } from '../test/coordinator'
import { Layout } from './Layout'

vi.mock('../session/SessionProvider', () => ({
  useSession: () => baseSession,
  useSessionContext: () => ({ logout: vi.fn() }),
}))
vi.mock('../state/StoreProvider', () => ({
  useStore: () => ({ state: initialState, loadError: null, reload: vi.fn(), reconnect: vi.fn() }),
}))

function Navigation() {
  const navigate = useNavigate()
  const { pathname } = useLocation()
  return (
    <>
      <h1>{pathname}</h1>
      <button onClick={() => navigate('/jobs/new')}>Submit job</button>
      <button onClick={() => navigate('/jobs/replacement', { replace: true })}>Replace page</button>
      <button onClick={() => navigate(-1)}>Back</button>
    </>
  )
}

it('starts pushed and replaced pages at the top while leaving back navigation alone', async () => {
  const scroll = vi.spyOn(window, 'scrollTo').mockImplementation(() => {})
  const user = userEvent.setup()
  render(<MemoryRouter initialEntries={['/run']}><Layout><Navigation /></Layout></MemoryRouter>)
  expect(scroll).not.toHaveBeenCalled()

  await user.click(screen.getByRole('button', { name: 'Submit job' }))
  expect(screen.getByRole('heading', { name: '/jobs/new' })).toBeInTheDocument()
  expect(scroll).toHaveBeenCalledExactlyOnceWith(0, 0)
  await user.click(screen.getByRole('button', { name: 'Replace page' }))
  expect(screen.getByRole('heading', { name: '/jobs/replacement' })).toBeInTheDocument()
  expect(scroll).toHaveBeenCalledTimes(2)

  await user.click(screen.getByRole('button', { name: 'Back' }))
  expect(screen.getByRole('heading', { name: '/run' })).toBeInTheDocument()
  expect(scroll).toHaveBeenCalledTimes(2)
})
