import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Script } from '../api/types'
import { script } from '../test/coordinator'
import { apiError, device, job, mockFetch, target, type Call } from '../test/helpers'
import { ScriptComposer } from './ScriptComposer'

const library: Script[] = [
  script('s1', 'Clean temp files', { shell: 'cmd', body: 'del /q %TEMP%\\*' }),
  script('s2', 'Show IP', { body: 'Get-NetIPAddress' }),
  script('s3', 'Check disk', { built_in: true, body: 'Get-Volume' }),
]

function setup(reply?: Parameters<typeof mockFetch>[0]) {
  const calls = mockFetch(
    reply ??
      ((call) => {
        if (call.method === 'GET' && call.path === '/scripts') return { body: { scripts: library } }
        if (call.method === 'POST' && call.path === '/scripts') {
          const input = call.body as { name: string; shell: 'powershell' | 'cmd'; body: string }
          return { status: 201, body: { script: script('new', input.name, input) } }
        }
        if (call.path === '/jobs') return { status: 201, body: { job: job('j', [target('t', 'a')]) } }
        return undefined
      }),
  )
  const user = userEvent.setup()
  render(
    <ScriptComposer
      targets={{ type: 'devices', device_ids: ['a'] }}
      devices={[device('a', { hostname: 'LAB-01' })]}
      onCreated={() => {}}
    />,
  )
  return { calls, user }
}

const body = () => screen.getByRole('textbox', { name: 'Script body' })
const posts = (calls: Call[], path: string) => calls.filter((c) => c.method === 'POST' && c.path === path)

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('Load from library', () => {
  it('lists the library with shell and built-in badge, and fills the form from the chosen script', async () => {
    const { user } = setup()
    await user.type(body(), 'something typed before')
    await user.click(screen.getByRole('button', { name: /Load from library/ }))

    const dialog = await screen.findByRole('dialog', { name: 'Load from library' })
    const items = await within(dialog).findAllByRole('button', { name: /Clean temp|Show IP|Check disk/ })
    expect(items.map((i) => i.textContent)).toEqual(['Clean temp filescmd', 'Show IPPowerShell', 'Check diskPowerShellbuilt-in'])

    await user.click(items[0]!)
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(body()).toHaveValue('del /q %TEMP%\\*')
    expect(screen.getByRole('radio', { name: 'cmd' })).toBeChecked()
    expect(screen.getByRole('status')).toHaveTextContent('Loaded “Clean temp files” from the library.')
  })

  it('filters by name', async () => {
    const { user } = setup()
    await user.click(screen.getByRole('button', { name: /Load from library/ }))
    const dialog = await screen.findByRole('dialog')
    await within(dialog).findByRole('button', { name: /Show IP/ })
    await user.type(within(dialog).getByRole('searchbox'), 'disk')
    expect(within(dialog).getAllByRole('button', { name: /Check disk|Show IP|Clean/ })).toHaveLength(1)
  })

  it('says so when the library is empty', async () => {
    const { user } = setup((call) => (call.path === '/scripts' ? { body: { scripts: [] } } : undefined))
    await user.click(screen.getByRole('button', { name: /Load from library/ }))
    expect(await screen.findByText(/The library is empty/)).toBeInTheDocument()
  })

  it('shows why the library could not be read', async () => {
    const { user } = setup(() => apiError(500, 'internal', 'database is locked'))
    await user.click(screen.getByRole('button', { name: /Load from library/ }))
    expect(await screen.findByRole('alert')).toHaveTextContent('database is locked')
  })

  it('can be dismissed with Escape, leaving the form alone', async () => {
    const { user } = setup()
    await user.type(body(), 'keep me')
    await user.click(screen.getByRole('button', { name: /Load from library/ }))
    await screen.findByRole('dialog')
    await user.keyboard('{Escape}')
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(body()).toHaveValue('keep me')
  })
})

describe('Save to library', () => {
  it('saves the shell and body under the given name', async () => {
    const { calls, user } = setup()
    await user.click(screen.getByRole('radio', { name: 'cmd' }))
    await user.type(body(), 'echo hi')
    await user.click(screen.getByRole('button', { name: /Save to library/ }))
    await user.type(screen.getByRole('textbox', { name: 'Name' }), '  Say hi ')
    await user.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => expect(posts(calls, '/scripts')).toHaveLength(1))
    expect(posts(calls, '/scripts')[0]?.body).toEqual({ name: 'Say hi', shell: 'cmd', body: 'echo hi' })
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(screen.getByRole('status')).toHaveTextContent('Saved to the library as “Say hi”.')
  })

  it('does not save an empty script or an empty name', async () => {
    const { calls, user } = setup()
    await user.click(screen.getByRole('button', { name: /Save to library/ }))
    await user.click(screen.getByRole('button', { name: 'Save' }))
    const alerts = within(screen.getByRole('dialog')).getAllByRole('alert').map((a) => a.textContent?.trim())
    expect(alerts).toEqual(['Give the script a name.', 'The script is empty.'])
    expect(posts(calls, '/scripts')).toHaveLength(0)
  })

  it('reports a taken name next to the field and lets the user choose another', async () => {
    const { calls, user } = setup((call) => {
      if (call.method === 'POST' && call.path === '/scripts') {
        return (call.body as { name: string }).name === 'Show IP'
          ? apiError(409, 'name_taken')
          : { status: 201, body: { script: script('n', 'Show IP 2') } }
      }
      return undefined
    })
    await user.type(body(), 'Get-NetIPAddress')
    await user.click(screen.getByRole('button', { name: /Save to library/ }))
    const name = screen.getByRole('textbox', { name: 'Name' })
    await user.type(name, 'Show IP')
    await user.click(screen.getByRole('button', { name: 'Save' }))

    expect(await screen.findByRole('alert')).toHaveTextContent('A script named “Show IP” already exists. Choose another name.')
    expect(name).toHaveAttribute('aria-invalid', 'true')
    expect(screen.getByRole('dialog')).toBeInTheDocument()

    await user.type(name, ' 2')
    await user.click(screen.getByRole('button', { name: 'Save' }))
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
    expect(posts(calls, '/scripts')).toHaveLength(2)
  })

  it('does not run the script when saving it', async () => {
    const { calls, user } = setup()
    await user.type(body(), 'echo')
    await user.click(screen.getByRole('button', { name: /Save to library/ }))
    await user.type(screen.getByRole('textbox', { name: 'Name' }), 'X')
    await user.click(screen.getByRole('button', { name: 'Save' }))
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
    expect(posts(calls, '/jobs')).toHaveLength(0)
  })
})
