import { useEffect, useMemo, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { api, errorMessage } from '../api/client'
import type { Updates } from '../api/types'
import { ErrorNote } from '../components/bits'
import { formatBytes, formatDateTime, plural, relativeTime, useNow } from '../lib/format'
import type { ComposeRequest } from '../lib/targets'
import { useStore } from '../state/StoreProvider'
import { sortedDevices } from '../state/store'

export function UpdatesPage() {
  const { state, reload } = useStore()
  const navigate = useNavigate()
  const now = useNow(30_000)
  const [updates, setUpdates] = useState<Updates | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [checking, setChecking] = useState(false)
  const [checkError, setCheckError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    api.getUpdates().then(
      (next) => {
        if (cancelled) return
        setUpdates(next)
        setError(null)
      },
      (err: unknown) => {
        if (!cancelled) setError(errorMessage(err))
      },
    )
    return () => {
      cancelled = true
    }
  }, [state.syncEpoch])

  const outdated = useMemo(() => sortedDevices(state).filter((d) => d.update_available === true), [state.devices])

  async function checkNow() {
    if (checking) return
    setChecking(true)
    setCheckError(null)
    try {
      setUpdates(await api.checkUpdates())
    } catch (err) {
      setCheckError(errorMessage(err))
      // The coordinator records the error too (last_error): show what it now holds.
      try {
        setUpdates(await api.getUpdates())
      } catch {
        // Keep what is shown.
      }
    } finally {
      setChecking(false)
    }
  }

  function updateOutdated() {
    const request: ComposeRequest = { targets: { type: 'devices', device_ids: outdated.map((d) => d.id) } }
    navigate('/update', { state: request })
  }

  return (
    <>
      <div className="page-head">
        <h1>Updates</h1>
        <span className="toolbar-spacer" />
        <button type="button" onClick={() => void checkNow()} disabled={checking || (updates !== null && !updates.enabled)}>
          {checking ? 'Checking…' : 'Check now'}
        </button>
      </div>
      <ErrorNote message={error} onRetry={reload} />
      <ErrorNote message={checkError} />

      {updates === null ? (
        error ? null : <p className="muted">Loading&hellip;</p>
      ) : (
        <>
          {!updates.enabled ? (
            <div className="notice" role="note">
              <p>
                <strong>Update checking is off.</strong> This build of the coordinator has no update signing key built
                in, or checking has been turned off, so it cannot look for new client versions. Versions that were
                downloaded earlier can still be installed.
              </p>
            </div>
          ) : null}

          <section className="panel" aria-labelledby="source">
            <h2 id="source">Release source</h2>
            <dl className="fields">
              <div>
                <dt>Checking</dt>
                <dd>{updates.enabled ? 'on' : 'off'}</dd>
              </div>
              <div>
                <dt>Source</dt>
                <dd>{updates.source ? <span className="mono">{updates.source}</span> : <span className="muted">none</span>}</dd>
              </div>
              <div>
                <dt>Last checked</dt>
                <dd title={updates.last_checked_at !== null ? formatDateTime(updates.last_checked_at) : undefined}>
                  {updates.last_checked_at !== null ? relativeTime(updates.last_checked_at, now) : <span className="muted">never</span>}
                </dd>
              </div>
            </dl>
            {updates.last_error ? (
              <p className="error" role="alert">
                Last check failed: {updates.last_error}
              </p>
            ) : null}
          </section>

          <section className="panel" aria-labelledby="newest">
            <h2 id="newest">Newest client version</h2>
            {updates.latest ? (
              <>
                <p>
                  <strong>{updates.latest.version}</strong>{' '}
                  <span className="muted">
                    for {updates.latest.target}, {formatBytes(updates.latest.size)}, downloaded{' '}
                    {relativeTime(updates.latest.cached_at, now)}
                  </span>
                </p>
                {updates.latest.notes ? <p className="release-notes">{updates.latest.notes}</p> : null}
              </>
            ) : (
              <p className="muted">
                No client version has been downloaded yet.{' '}
                {updates.enabled ? 'Use “Check now” to look for one.' : ''}
              </p>
            )}
            <p>
              {outdated.length === 0 ? (
                <span className="muted">No device has an update available.</span>
              ) : (
                <>
                  <strong>{plural(outdated.length, 'device')}</strong> {outdated.length === 1 ? 'has' : 'have'} an
                  update available.
                </>
              )}
            </p>
            <div className="form-actions">
              <button type="button" className="primary" disabled={outdated.length === 0} onClick={updateOutdated}>
                Update all outdated devices&hellip;
              </button>
            </div>
          </section>

          <section aria-labelledby="cached">
            <h2 id="cached">Downloaded versions</h2>
            {updates.cached.length === 0 ? (
              <p className="muted">None.</p>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>Version</th>
                    <th>Target</th>
                    <th>Size</th>
                    <th>Downloaded</th>
                    <th>Notes</th>
                  </tr>
                </thead>
                <tbody>
                  {updates.cached.map((release) => (
                    <tr key={`${release.version}-${release.target}`}>
                      <td>{release.version}</td>
                      <td>{release.target}</td>
                      <td className="nowrap">{formatBytes(release.size)}</td>
                      <td className="nowrap" title={formatDateTime(release.cached_at)}>
                        {relativeTime(release.cached_at, now)}
                      </td>
                      <td className="col-notes">{release.notes ? <span className="release-notes">{release.notes}</span> : <span className="muted">&mdash;</span>}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </section>
        </>
      )}
    </>
  )
}
