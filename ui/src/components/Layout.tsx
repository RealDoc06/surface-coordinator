import { useState, type ReactNode } from 'react'
import { NavLink } from 'react-router-dom'
import { useSession, useSessionContext } from '../session/SessionProvider'
import { useStore } from '../state/StoreProvider'
import { ErrorNote } from './bits'

export function Layout({ children }: { children: ReactNode }) {
  const session = useSession()
  const { logout } = useSessionContext()
  const { state, loadError, reload } = useStore()
  const [loggingOut, setLoggingOut] = useState(false)

  async function onLogout() {
    setLoggingOut(true)
    try {
      await logout()
    } finally {
      setLoggingOut(false)
    }
  }

  return (
    <div className="app">
      <header className="app-header">
        <span className="brand">Surface Coordinator</span>
        <nav aria-label="Main">
          <NavLink to="/" end>
            Devices
          </NavLink>
          <NavLink to="/jobs">Jobs</NavLink>
          <NavLink to="/keys">Enrollment keys</NavLink>
          <NavLink to="/rejected">
            Rejected connections
            {state.rejectedUnseen > 0 ? (
              <span className="nav-badge" aria-label={`${state.rejectedUnseen} new`}>
                {state.rejectedUnseen > 99 ? '99+' : state.rejectedUnseen}
              </span>
            ) : null}
          </NavLink>
          {session.capabilities.accounts ? <NavLink to="/accounts">Accounts</NavLink> : null}
        </nav>
        <span className="header-spacer" />
        <span className="muted version" title={`Protocol ${session.protocol_version}`}>
          v{session.version}
        </span>
        {session.account ? (
          <>
            <span className="username">{session.account.username}</span>
            <button type="button" onClick={() => void onLogout()} disabled={loggingOut}>
              Log out
            </button>
          </>
        ) : null}
      </header>
      {state.connection === 'reconnecting' ? (
        <div className="banner" role="status">
          Live updates paused, reconnecting&hellip;
        </div>
      ) : null}
      <main className="app-main">
        <ErrorNote message={loadError} onRetry={reload} />
        {children}
      </main>
    </div>
  )
}
