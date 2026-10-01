import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import { api, errorMessage, setUnauthorizedHandler } from '../api/client'
import type { Session } from '../api/types'

/** Reads `token=...` from a URL fragment such as `#token=abc`. */
export function tokenFromHash(hash: string): string | null {
  const fragment = hash.startsWith('#') ? hash.slice(1) : hash
  const token = new URLSearchParams(fragment).get('token')
  return token !== null && token !== '' ? token : null
}

type Boot = { session: Session; tokenError: string | null }

/**
 * Startup: in token mode the manager opens the UI at `/#token=...`. Exchange
 * it for the session cookie and drop it from the address bar, then ask the
 * coordinator who we are.
 */
async function boot(): Promise<Boot> {
  let tokenError: string | null = null
  const token = tokenFromHash(window.location.hash)
  if (token !== null) {
    window.history.replaceState(
      window.history.state,
      '',
      window.location.pathname + window.location.search,
    )
    try {
      await api.exchangeToken(token)
    } catch (err) {
      tokenError = errorMessage(err)
    }
  }
  return { session: await api.getSession(), tokenError }
}

export type SessionStatus =
  | { phase: 'loading' }
  | { phase: 'error'; message: string }
  | { phase: 'ready'; session: Session; tokenError: string | null }

type SessionContextValue = {
  status: SessionStatus
  /** Ask the coordinator again who we are (after login, or when in doubt). */
  refresh: () => Promise<void>
  logout: () => Promise<void>
  retry: () => void
}

const SessionContext = createContext<SessionContextValue | null>(null)

export function SessionProvider({ children }: { children: ReactNode }) {
  const [status, setStatus] = useState<SessionStatus>({ phase: 'loading' })
  // Kept in a ref so that a repeated effect run (React StrictMode) reuses the
  // one startup sequence instead of racing a second one against it.
  const booting = useRef<Promise<Boot> | null>(null)

  const start = useCallback(() => {
    booting.current ??= boot()
    const current = booting.current
    current.then(
      ({ session, tokenError }) => {
        if (booting.current === current) setStatus({ phase: 'ready', session, tokenError })
      },
      (err: unknown) => {
        if (booting.current === current) setStatus({ phase: 'error', message: errorMessage(err) })
      },
    )
  }, [])

  useEffect(() => {
    start()
  }, [start])

  const refresh = useCallback(async () => {
    try {
      const session = await api.getSession()
      setStatus((prev) => ({
        phase: 'ready',
        session,
        tokenError: prev.phase === 'ready' && !session.authenticated ? prev.tokenError : null,
      }))
    } catch {
      // Unreachable coordinator: keep what we have; the views show their own errors.
    }
  }, [])

  const logout = useCallback(async () => {
    try {
      await api.logout()
    } finally {
      await refresh()
    }
  }, [refresh])

  const retry = useCallback(() => {
    booting.current = null
    setStatus({ phase: 'loading' })
    start()
  }, [start])

  useEffect(() => {
    setUnauthorizedHandler(() => {
      setStatus((prev) =>
        prev.phase === 'ready' && prev.session.authenticated
          ? { ...prev, session: { ...prev.session, authenticated: false, account: null } }
          : prev,
      )
    })
    return () => setUnauthorizedHandler(null)
  }, [])

  const value = useMemo(() => ({ status, refresh, logout, retry }), [status, refresh, logout, retry])
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>
}

export function useSessionContext(): SessionContextValue {
  const value = useContext(SessionContext)
  if (!value) throw new Error('useSessionContext must be used inside SessionProvider')
  return value
}

/** The session of the signed-in user; only for components rendered after startup. */
export function useSession(): Session {
  const { status } = useSessionContext()
  if (status.phase !== 'ready') throw new Error('useSession used before the session is known')
  return status.session
}
