import { useEffect, useId, useMemo, useState, type FormEvent } from 'react'
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import type { Script, Shell } from '../api/types'
import { ErrorNote } from '../components/bits'
import { ConfirmDialog } from '../components/Modal'
import { SHELL_LABELS } from '../lib/jobKinds'
import { copyName, MAX_SCRIPT_NAME_CHARS, validateScript, type ScriptErrors } from '../lib/scripts'
import { parseComposeRequest } from '../lib/targets'
import { useStore } from '../state/StoreProvider'

type Prefill = { name: string; shell: Shell; body: string }

function parsePrefill(state: unknown): Prefill | null {
  if (typeof state !== 'object' || state === null) return null
  const raw = (state as { prefill?: unknown }).prefill
  if (typeof raw !== 'object' || raw === null) return null
  const p = raw as Record<string, unknown>
  if (typeof p.name !== 'string' || typeof p.body !== 'string') return null
  return { name: p.name, shell: p.shell === 'cmd' ? 'cmd' : 'powershell', body: p.body }
}

/** `/scripts/new` creates a script, `/scripts/:id` shows, edits or deletes one. */
export function ScriptEditPage() {
  const { id } = useParams()
  const location = useLocation()
  if (id === undefined || id === 'new') {
    return <ScriptEditor key={location.key} script={null} prefill={parsePrefill(location.state)} />
  }
  return <ScriptLoader key={id} id={id} />
}

function ScriptLoader({ id }: { id: string }) {
  const { state, reload } = useStore()
  const location = useLocation()
  const request = useMemo(() => parseComposeRequest(location.state), [location.state])
  const [script, setScript] = useState<Script | null>(null)
  const [missing, setMissing] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // There is no route for one script: it is found in the list.
  useEffect(() => {
    let cancelled = false
    api.listScripts().then(
      ({ scripts }) => {
        if (cancelled) return
        const found = scripts.find((s) => s.id === id)
        if (found) {
          setScript(found)
          setMissing(false)
        } else setMissing(true)
        setError(null)
      },
      (err: unknown) => {
        if (!cancelled) setError(errorMessage(err))
      },
    )
    return () => {
      cancelled = true
    }
  }, [id, state.syncEpoch])

  if (missing && !script) {
    return (
      <div className="empty">
        <h2>Script not found</h2>
        <p>
          It may have been deleted.{' '}
          <Link to="/scripts" state={{ targets: request.targets }}>
            Back to scripts
          </Link>
        </p>
      </div>
    )
  }
  if (!script) {
    return error ? <ErrorNote message={error} onRetry={reload} /> : <p className="muted">Loading script&hellip;</p>
  }
  return <ScriptEditor key={script.id} script={script} prefill={null} onSaved={setScript} />
}

function ScriptEditor({
  script,
  prefill,
  onSaved,
}: {
  script: Script | null
  prefill: Prefill | null
  onSaved?: (script: Script) => void
}) {
  const navigate = useNavigate()
  const location = useLocation()
  const request = useMemo(() => parseComposeRequest(location.state), [location.state])
  const ids = useId()
  const readOnly = script?.built_in === true

  const [name, setName] = useState(script?.name ?? prefill?.name ?? '')
  const [shell, setShell] = useState<Shell>(script?.shell ?? prefill?.shell ?? 'powershell')
  const [body, setBody] = useState(script?.body ?? prefill?.body ?? '')
  const [errors, setErrors] = useState<ScriptErrors>({})
  const [apiError, setApiError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [saved, setSaved] = useState(false)
  const [deleting, setDeleting] = useState(false)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  const dirty = script ? name !== script.name || shell !== script.shell || body !== script.body : true

  async function save(event: FormEvent) {
    event.preventDefault()
    if (saving || readOnly) return
    const found = validateScript({ name, shell, body })
    setErrors(found)
    setApiError(null)
    setSaved(false)
    if (found.name || found.body) return
    setSaving(true)
    try {
      const input = { name: name.trim(), shell, body }
      if (script) {
        const { script: updated } = await api.updateScript(script.id, input)
        onSaved?.(updated)
        setName(updated.name)
        setSaved(true)
      } else {
        await api.createScript(input)
        navigate('/scripts', { state: { targets: request.targets } })
      }
    } catch (err) {
      if (err instanceof ApiError && err.code === 'name_taken') {
        setErrors({ name: `A script named “${name.trim()}” already exists. Choose another name.` })
      } else {
        setApiError(errorMessage(err))
      }
    } finally {
      setSaving(false)
    }
  }

  async function onDelete() {
    if (!script) return
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      await api.deleteScript(script.id)
      navigate('/scripts', { state: { targets: request.targets } })
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) navigate('/scripts', { state: { targets: request.targets } })
      else {
        setDeleteError(errorMessage(err))
        setDeleteBusy(false)
      }
    }
  }

  return (
    <>
      <div className="page-head">
        <h1>{script ? script.name : 'New script'}</h1>
        {readOnly ? <span className="badge">built-in</span> : null}
        <span className="toolbar-spacer" />
        {script ? (
          <>
            <button
              type="button"
              className="primary"
              disabled={dirty && !readOnly}
              title={dirty && !readOnly ? 'Save your changes first' : undefined}
              onClick={() =>
                navigate('/run', {
                  state: {
                    payload: { shell: script.shell, body: script.body },
                    targets: request.targets ?? { type: 'all' },
                  },
                })
              }
            >
              Run&hellip;
            </button>
            <button
              type="button"
              onClick={() =>
                navigate('/scripts/new', {
                  state: {
                    prefill: { name: copyName(script.name), shell: script.shell, body: script.body },
                    targets: request.targets,
                  },
                })
              }
            >
              Duplicate
            </button>
            {readOnly ? null : (
              <button type="button" className="danger" onClick={() => setDeleting(true)}>
                Delete
              </button>
            )}
          </>
        ) : null}
      </div>
      <p className="muted">
        <Link to="/scripts" state={{ targets: request.targets }}>
          All scripts
        </Link>
      </p>
      {readOnly ? (
        <p className="notice" role="note">
          Built-in scripts are read-only. Duplicate it to make a copy you can change.
        </p>
      ) : null}

      <form className="composer" onSubmit={(e) => void save(e)} noValidate>
        <section className="panel">
          <div className="field">
            <label htmlFor={`${ids}-name`}>Name</label>
            <input
              id={`${ids}-name`}
              type="text"
              className="input-wide"
              autoComplete="off"
              readOnly={readOnly}
              value={name}
              onChange={(e) => {
                setName(e.target.value)
                setSaved(false)
              }}
              aria-invalid={errors.name ? true : undefined}
              aria-describedby={errors.name ? `${ids}-name-error` : undefined}
            />
            {errors.name ? (
              <p id={`${ids}-name-error`} className="error" role="alert">
                {errors.name}
              </p>
            ) : (
              <p className="hint">Up to {MAX_SCRIPT_NAME_CHARS} characters.</p>
            )}
          </div>

          <fieldset className="choice">
            <legend>Shell</legend>
            {(Object.keys(SHELL_LABELS) as Shell[]).map((value) => (
              <label key={value}>
                <input
                  type="radio"
                  name={`${ids}-shell`}
                  checked={shell === value}
                  disabled={readOnly}
                  onChange={() => {
                    setShell(value)
                    setSaved(false)
                  }}
                />
                {SHELL_LABELS[value]}
              </label>
            ))}
          </fieldset>

          <div className="field">
            <label htmlFor={`${ids}-body`}>Script body</label>
            <textarea
              id={`${ids}-body`}
              className="mono script-body"
              rows={16}
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              readOnly={readOnly}
              value={body}
              onChange={(e) => {
                setBody(e.target.value)
                setSaved(false)
              }}
              aria-invalid={errors.body ? true : undefined}
              aria-describedby={errors.body ? `${ids}-body-error` : undefined}
            />
            {errors.body ? (
              <p id={`${ids}-body-error`} className="error" role="alert">
                {errors.body}
              </p>
            ) : null}
          </div>
        </section>

        <ErrorNote message={apiError} />

        {readOnly ? null : (
          <div className="form-actions">
            <button type="submit" className="primary" disabled={saving || (script !== null && !dirty)}>
              {saving ? 'Saving…' : script ? 'Save changes' : 'Create script'}
            </button>
            <button type="button" onClick={() => navigate('/scripts', { state: { targets: request.targets } })}>
              {script ? 'Close' : 'Cancel'}
            </button>
            {saved && !dirty ? (
              <span className="success" role="status">
                Saved.
              </span>
            ) : null}
          </div>
        )}
      </form>

      {deleting && script ? (
        <ConfirmDialog
          title={`Delete “${script.name}”?`}
          confirmLabel="Delete script"
          danger
          busy={deleteBusy}
          error={deleteError}
          onConfirm={() => void onDelete()}
          onCancel={() => {
            setDeleting(false)
            setDeleteError(null)
          }}
        >
          <p>The script is removed from the library. Jobs that already ran it keep their own copy.</p>
        </ConfirmDialog>
      ) : null}
    </>
  )
}
