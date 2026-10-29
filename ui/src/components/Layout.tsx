import { useLayoutEffect, useState, type ReactNode } from 'react'
import { errorMessage } from '../api/client'
import { NavLink, useLocation, useNavigationType } from 'react-router-dom'
import { useSession, useSessionContext } from '../session/SessionProvider'
import { useStore } from '../state/StoreProvider'
import { ErrorNote } from './bits'
import { MenuButton, type MenuItem } from './MenuButton'

function rejectedBadge(count: number): string {
  return count > 99 ? '99+' : String(count)
}

export function Layout({ children }: { children: ReactNode }) {
  const session = useSession()
  const { logout } = useSessionContext()
  const { state, loadError, reload, reconnect } = useStore()
  const [loggingOut, setLoggingOut] = useState(false)
  const [logoutError, setLogoutError] = useState<string | null>(null)
  const { pathname } = useLocation()
  const navigationType = useNavigationType()

  useLayoutEffect(() => {
    // New pages start at their heading; back/forward keeps browser restoration.
    if (navigationType !== 'POP') window.scrollTo(0, 0)
  }, [pathname, navigationType])

  const outdated = Object.values(state.devices).filter((d) => d.update_available === true).length

  // Everything that is set up rather than used day to day lives in one menu.
  const admin: MenuItem[] = []
  if (session.capabilities.hotspot) admin.push({ key: 'hotspot', label: 'Hotspot', to: '/hotspot' })
  admin.push({ key: 'keys', label: 'Enrollment keys', to: '/keys' })
  admin.push({
    key: 'rejected',
    label: 'Rejected connections',
    to: '/rejected',
    badge:
      state.rejectedUnseen > 0 ? (
        <span className="nav-badge">{rejectedBadge(state.rejectedUnseen)}</span>
      ) : undefined,
  })
  if (session.capabilities.accounts) admin.push({ key: 'accounts', label: 'Accounts', to: '/accounts' })
  const adminActive = admin.some((item) => 'to' in item && item.to && pathname.startsWith(item.to))

  async function onLogout() {
    setLoggingOut(true)
    setLogoutError(null)
    try {
      await logout()
    } catch (err) {
      setLogoutError(errorMessage(err))
    } finally {
      setLoggingOut(false)
    }
  }

  return (
    <div className="app">
      <header className="app-header">
        <NavLink className="brand" to="/">Surface Coordinator</NavLink>
        <nav aria-label="Main">
          <NavLink to="/" end>
            Devices
          </NavLink>
          <NavLink to="/jobs">Jobs</NavLink>
          <NavLink to="/groups">Groups</NavLink>
          <NavLink to="/scripts">Scripts</NavLink>
          <NavLink to="/updates">
            Updates
            {outdated > 0 ? (
              <span className="nav-badge info" aria-label={`${outdated} outdated`}>
                {outdated > 99 ? '99+' : outdated}
              </span>
            ) : null}
          </NavLink>
          <MenuButton
            label="Administration"
            className={adminActive ? 'nav-menu active' : 'nav-menu'}
            items={admin}
            adornment={
              state.rejectedUnseen > 0 ? (
                <span className="nav-badge" aria-label={`${state.rejectedUnseen} new`}>
                  {rejectedBadge(state.rejectedUnseen)}
                </span>
              ) : undefined
            }
          />
        </nav>
        <span className="header-spacer" />
        <span className={`connection-state ${state.connection === 'open' ? 'is-live' : ''}`} title="Connection between this browser and the coordinator">
          <span className={`dot ${state.connection === 'open' ? 'dot-online' : 'dot-offline'}`} aria-hidden="true" />
          {state.connection === 'open' ? 'Live' : state.connection === 'connecting' ? 'Connecting' : 'Reconnecting'}
        </span>
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
          <span>Live updates paused, reconnecting&hellip; Device status may be out of date.</span>
          <button type="button" onClick={reconnect}>Reconnect now</button>
        </div>
      ) : null}
      <main className="app-main">
        <ErrorNote message={logoutError} />
        <ErrorNote message={loadError} onRetry={reload} />
        {children}
      </main>
    </div>
  )
}
