import { BrowserRouter, Link, Route, Routes } from 'react-router-dom'
import { Layout } from './components/Layout'
import { AccountsPage } from './pages/AccountsPage'
import { ComposePage } from './pages/ComposePage'
import { DeviceDetailPage } from './pages/DeviceDetailPage'
import { DevicesPage } from './pages/DevicesPage'
import { JobDetailPage } from './pages/JobDetailPage'
import { JobsPage } from './pages/JobsPage'
import { HotspotPage } from './pages/HotspotPage'
import { KeysPage } from './pages/KeysPage'
import { LoginPage, TokenNotice } from './pages/LoginPage'
import { RejectedPage } from './pages/RejectedPage'
import { WifiPage } from './pages/WifiPage'
import { SessionProvider, useSessionContext } from './session/SessionProvider'
import { StoreProvider } from './state/StoreProvider'

export function App() {
  return (
    <SessionProvider>
      <Gate />
    </SessionProvider>
  )
}

/** Decides between the startup states, the sign-in screens and the application. */
function Gate() {
  const { status, retry, refresh } = useSessionContext()

  if (status.phase === 'loading') {
    return (
      <main className="center-page">
        <p className="muted">Connecting to the coordinator&hellip;</p>
      </main>
    )
  }
  if (status.phase === 'error') {
    return (
      <main className="center-page">
        <div className="panel login">
          <h1>Surface Coordinator</h1>
          <p className="error" role="alert">
            {status.message}
          </p>
          <button type="button" className="primary" onClick={retry}>
            Try again
          </button>
        </div>
      </main>
    )
  }
  if (!status.session.authenticated) {
    return status.session.auth_mode === 'accounts' ? <LoginPage /> : <TokenNotice error={status.tokenError} />
  }

  return (
    // When the events socket drops, check that the session is still valid: an
    // expired session shows the sign-in screen instead of reconnecting forever.
    <StoreProvider onDisconnected={() => void refresh()}>
      <BrowserRouter>
        <Layout>
          <Routes>
            <Route path="/" element={<DevicesPage />} />
            <Route path="/devices/:id" element={<DeviceDetailPage />} />
            <Route path="/run" element={<ComposePage />} />
            <Route path="/wifi" element={<WifiPage />} />
            <Route path="/jobs" element={<JobsPage />} />
            <Route path="/jobs/:id" element={<JobDetailPage />} />
            <Route path="/keys" element={<KeysPage />} />
            <Route path="/hotspot" element={<HotspotPage />} />
            <Route path="/rejected" element={<RejectedPage />} />
            <Route path="/accounts" element={<AccountsPage />} />
            <Route path="*" element={<NotFound />} />
          </Routes>
        </Layout>
      </BrowserRouter>
    </StoreProvider>
  )
}

function NotFound() {
  return (
    <div className="empty">
      <h2>Page not found</h2>
      <p>
        <Link to="/">Back to devices</Link>
      </p>
    </div>
  )
}
