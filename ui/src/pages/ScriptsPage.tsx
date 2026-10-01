import { useEffect, useMemo, useState } from 'react'
import { Link, useLocation, useNavigate } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import type { Script } from '../api/types'
import { ErrorNote } from '../components/bits'
import { ConfirmDialog } from '../components/Modal'
import { TargetSummary } from '../components/TargetSummary'
import { relativeTime, useNow } from '../lib/format'
import { SHELL_LABELS } from '../lib/jobKinds'
import { copyName, scriptsByName } from '../lib/scripts'
import { parseComposeRequest, type ComposeRequest } from '../lib/targets'
import { useStore } from '../state/StoreProvider'
import { sortedDevices } from '../state/store'

export function ScriptsPage() {
  const { state, reload } = useStore()
  const navigate = useNavigate()
  const location = useLocation()
  const now = useNow(30_000)
  const request = useMemo(() => parseComposeRequest(location.state), [location.state])
  const devices = useMemo(() => sortedDevices(state), [state.devices])

  const [scripts, setScripts] = useState<Script[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<Script | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  async function load() {
    try {
      const { scripts } = await api.listScripts()
      setScripts(scriptsByName(scripts))
      setError(null)
    } catch (err) {
      setError(errorMessage(err))
    }
  }

  useEffect(() => {
    let cancelled = false
    api.listScripts().then(
      ({ scripts }) => {
        if (cancelled) return
        setScripts(scriptsByName(scripts))
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

  /** Where the script is run: the selection the user came with, or all devices. */
  const runTargets = request.targets ?? ({ type: 'all' } as const)

  function run(script: Script) {
    const next: ComposeRequest = { payload: { shell: script.shell, body: script.body }, targets: runTargets }
    navigate('/run', { state: next })
  }

  function duplicate(script: Script) {
    navigate('/scripts/new', {
      state: { prefill: { name: copyName(script.name), shell: script.shell, body: script.body }, targets: request.targets },
    })
  }

  async function onDelete() {
    if (!deleting) return
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      await api.deleteScript(deleting.id)
      setDeleting(null)
      await load()
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) {
        setDeleting(null)
        await load()
      } else {
        setDeleteError(errorMessage(err))
      }
    } finally {
      setDeleteBusy(false)
    }
  }

  return (
    <>
      <div className="page-head">
        <h1>Scripts</h1>
        <span className="toolbar-spacer" />
        <Link className="button primary" to="/scripts/new" state={{ targets: request.targets }}>
          New script
        </Link>
      </div>
      <p className="lead">
        Scripts you reuse. Running one copies it into the job, so editing or deleting a library script never changes what
        an earlier job did.
      </p>

      {request.targets ? (
        <section className="panel" aria-label="Where scripts will run">
          <h2>Run on</h2>
          <TargetSummary spec={request.targets} devices={devices} groups={state.groups} />
          <p className="hint">Choose “Run…” on a script to continue with these devices.</p>
        </section>
      ) : null}

      <ErrorNote message={error} onRetry={reload} />
      {scripts === null ? (
        error ? null : <p className="muted">Loading scripts&hellip;</p>
      ) : scripts.length === 0 ? (
        <div className="empty">
          <h2>No scripts yet</h2>
          <p>
            Create one with “New script”, or save one from the composer with “Save to library…”.
          </p>
        </div>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Shell</th>
              <th>Updated</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {scripts.map((script) => (
              <tr key={script.id}>
                <td>
                  <Link to={`/scripts/${encodeURIComponent(script.id)}`}>{script.name}</Link>{' '}
                  {script.built_in ? <span className="badge">built-in</span> : null}
                </td>
                <td>{SHELL_LABELS[script.shell]}</td>
                <td className="nowrap">{script.built_in ? <span className="muted">&mdash;</span> : relativeTime(script.updated_at, now)}</td>
                <td className="row-actions left">
                  <button type="button" className="primary" aria-label={`Run ${script.name}`} onClick={() => run(script)}>
                    Run&hellip;
                  </button>
                  <button type="button" aria-label={`Duplicate ${script.name}`} onClick={() => duplicate(script)}>
                    Duplicate
                  </button>
                  {script.built_in ? null : (
                    <button
                      type="button"
                      className="danger"
                      aria-label={`Delete ${script.name}`}
                      onClick={() => setDeleting(script)}
                    >
                      Delete
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {deleting ? (
        <ConfirmDialog
          title={`Delete “${deleting.name}”?`}
          confirmLabel="Delete script"
          danger
          busy={deleteBusy}
          error={deleteError}
          onConfirm={() => void onDelete()}
          onCancel={() => {
            setDeleting(null)
            setDeleteError(null)
          }}
        >
          <p>The script is removed from the library. Jobs that already ran it keep their own copy.</p>
        </ConfirmDialog>
      ) : null}
    </>
  )
}
