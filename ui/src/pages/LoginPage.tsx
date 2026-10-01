import { useState, type FormEvent } from 'react'
import { api, errorMessage } from '../api/client'
import { useSessionContext } from '../session/SessionProvider'

export function LoginPage() {
  const { refresh } = useSessionContext()
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function onSubmit(event: FormEvent) {
    event.preventDefault()
    if (busy) return
    if (username === '' || password === '') {
      setError('Enter your username and password.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      await api.login(username, password)
      await refresh()
    } catch (err) {
      setError(errorMessage(err))
      setPassword('')
    } finally {
      setBusy(false)
    }
  }

  return (
    <main className="center-page">
      <form className="panel login" onSubmit={(e) => void onSubmit(e)} noValidate>
        <h1>Surface Coordinator</h1>
        <div className="field">
          <label htmlFor="login-username">Username</label>
          <input
            id="login-username"
            type="text"
            autoComplete="username"
            autoCapitalize="off"
            spellCheck={false}
            autoFocus
            value={username}
            onChange={(e) => setUsername(e.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor="login-password">Password</label>
          <input
            id="login-password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </div>
        {error ? (
          <p className="error" role="alert">
            {error}
          </p>
        ) : null}
        <button type="submit" className="primary" disabled={busy}>
          {busy ? 'Signing in…' : 'Sign in'}
        </button>
      </form>
    </main>
  )
}

/** Token mode without a valid token: only the manager can open the UI. */
export function TokenNotice({ error }: { error: string | null }) {
  return (
    <main className="center-page">
      <div className="panel login">
        <h1>Surface Coordinator</h1>
        <p>This coordinator runs inside the manager. Open this page from the manager application.</p>
        {error ? (
          <p className="error" role="alert">
            {error}
          </p>
        ) : null}
      </div>
    </main>
  )
}
