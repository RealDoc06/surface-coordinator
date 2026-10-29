import { useEffect, useMemo, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { api, errorMessage } from '../api/client'
import type { Updates } from '../api/types'
import { AutoUpdateSwitch, RolloutStatus } from '../components/AutoUpdates'
import { ErrorNote } from '../components/bits'
import { ClientUploadForm } from '../components/ClientUploadForm'
import { formatBytes, formatDateTime, plural, relativeTime, useNow } from '../lib/format'
import type { ComposeRequest } from '../lib/targets'
import { useStore } from '../state/StoreProvider'
import { sortedDevices } from '../state/store'

/** Where the coordinator looks for releases by itself, and how that last went. */
function ReleaseSource({ updates, now }: { updates: Updates; now: number }) {
  return (
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
  )
}

export function UpdatesPage() {
  const { state, reload } = useStore()
  const navigate = useNavigate()
  const now = useNow(30_000)
  const [updates, setUpdates] = useState<Updates | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [checking, setChecking] = useState(false)
  const [checkError, setCheckError] = useState<string | null>(null)
  const refreshUpdates = useRef<(() => void) | null>(null)
  const updatesRevision = useRef(0)

  function acceptUpdates(next: Updates) {
    // A response to an operator action supersedes an older background read.
    updatesRevision.current += 1
    setUpdates(next)
    setError(null)
  }

  useEffect(() => {
    let cancelled = false
    let timer: ReturnType<typeof setTimeout> | null = null
    let loading = false
    let dirty = false

    // Coalesce fleet event bursts and keep at most one request in flight.
    function schedule() {
      dirty = true
      if (cancelled || loading || timer !== null) return
      timer = setTimeout(() => {
        timer = null
        void load()
      }, 250)
    }

    async function load() {
      loading = true
      dirty = false
      const revision = updatesRevision.current
      try {
        const next = await api.getUpdates()
        if (!cancelled && revision === updatesRevision.current) {
          setUpdates(next)
          setError(null)
        }
      } catch (err) {
        if (!cancelled && revision === updatesRevision.current) setError(errorMessage(err))
      } finally {
        loading = false
        if (dirty && !cancelled) schedule()
      }
    }

    refreshUpdates.current = schedule
    void load()
    return () => {
      cancelled = true
      if (timer !== null) clearTimeout(timer)
      refreshUpdates.current = null
    }
  }, [state.syncEpoch])

  useEffect(() => {
    if (state.updatesEpoch > 0) refreshUpdates.current?.()
  }, [state.updatesEpoch])

  // Older coordinators do not say; they always check.
  const checksReleases = updates?.checking !== false

  const outdated = useMemo(() => sortedDevices(state).filter((d) => d.update_available === true), [state.devices])

  async function checkNow() {
    if (checking) return
    setChecking(true)
    setCheckError(null)
    try {
      acceptUpdates(await api.checkUpdates())
    } catch (err) {
      setCheckError(errorMessage(err))
      // The coordinator records the error too (last_error): show what it now holds.
      try {
        acceptUpdates(await api.getUpdates())
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
        {checksReleases ? (
          <button type="button" onClick={() => void checkNow()} disabled={checking || (updates !== null && !updates.enabled)}>
            {checking ? 'Checking…' : 'Check now'}
          </button>
        ) : null}
      </div>
      <ErrorNote message={error} onRetry={reload} />
      <ErrorNote message={checkError} />

      {updates === null ? (
        error ? null : <p className="muted">Loading&hellip;</p>
      ) : (
        <>
          {updates.rollout ? (
            <section className="panel" aria-labelledby="rollout">
              <h2 id="rollout">Rollout</h2>
              <RolloutStatus rollout={updates.rollout} />
            </section>
          ) : null}

          {updates.auto_update !== undefined ? (
            <section className="panel" aria-labelledby="auto">
              <h2 id="auto">Automatic updates</h2>
              <AutoUpdateSwitch enabled={updates.auto_update} onChanged={acceptUpdates} />
            </section>
          ) : null}

          {checksReleases ? (
            <ReleaseSource updates={updates} now={now} />
          ) : (
            <p className="muted">Updates are provided by upload; this coordinator does not check a release source.</p>
          )}

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
                {!updates.enabled ? '' : checksReleases ? 'Use “Check now” to look for one.' : 'Upload one below.'}
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

          <section className="panel" aria-labelledby="upload">
            <h2 id="upload">Upload a client build</h2>
            {updates.enabled ? (
              <ClientUploadForm onUploaded={acceptUpdates} />
            ) : (
              <p className="muted">
                Uploads are off: this coordinator has no update key built in, so it cannot verify client builds or
                install them on devices. Versions that were downloaded earlier can still be installed.
              </p>
            )}
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
