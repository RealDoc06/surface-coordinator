import { useEffect, useId, useState, type FormEvent } from 'react'
import { api, ApiError, errorMessage } from '../api/client'
import type { Script, Shell } from '../api/types'
import { SHELL_LABELS } from '../lib/jobKinds'
import { validateScript } from '../lib/scripts'
import { ErrorNote } from './bits'
import { Modal } from './Modal'

/** Choose a script from the library; it fills the composer's shell and body. */
export function ScriptPicker({ onPick, onClose }: { onPick: (script: Script) => void; onClose: () => void }) {
  const [scripts, setScripts] = useState<Script[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [filter, setFilter] = useState('')

  useEffect(() => {
    let cancelled = false
    api.listScripts().then(
      ({ scripts }) => {
        if (!cancelled) setScripts(scripts)
      },
      (err: unknown) => {
        if (!cancelled) setError(errorMessage(err))
      },
    )
    return () => {
      cancelled = true
    }
  }, [])

  const needle = filter.trim().toLowerCase()
  const shown = (scripts ?? []).filter((s) => needle === '' || s.name.toLowerCase().includes(needle))

  return (
    <Modal title="Load from library" wide onClose={onClose}>
      <p className="muted">Choosing a script replaces the shell and the script in the form.</p>
      <label className="field">
        <span className="visually-hidden">Filter scripts by name</span>
        <input
          type="search"
          data-autofocus
          placeholder="Filter by name"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
      </label>
      <ErrorNote message={error} />
      {scripts === null ? (
        error ? null : <p className="muted">Loading scripts&hellip;</p>
      ) : scripts.length === 0 ? (
        <p className="muted">The library is empty. Use “Save to library…” to add the current script.</p>
      ) : shown.length === 0 ? (
        <p className="muted">No script matches the filter.</p>
      ) : (
        <ul className="pick-list" aria-label="Scripts">
          {shown.map((script) => (
            <li key={script.id}>
              <button type="button" className="pick-item" onClick={() => onPick(script)}>
                <span className="pick-name">{script.name}</span>
                <span className="muted">{SHELL_LABELS[script.shell]}</span>
                {script.built_in ? <span className="badge">built-in</span> : null}
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="modal-actions">
        <button type="button" onClick={onClose}>
          Cancel
        </button>
      </div>
    </Modal>
  )
}

/** Ask for a name and save the composer's script to the library. */
export function SaveToLibraryDialog({
  shell,
  body,
  onSaved,
  onClose,
}: {
  shell: Shell
  body: string
  onSaved: (script: Script) => void
  onClose: () => void
}) {
  const [name, setName] = useState('')
  const [nameError, setNameError] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const id = useId()

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (busy) return
    const problems = validateScript({ name, shell, body })
    setNameError(problems.name ?? null)
    setError(problems.body ?? null)
    if (problems.name || problems.body) return
    setBusy(true)
    try {
      const { script } = await api.createScript({ name: name.trim(), shell, body })
      onSaved(script)
    } catch (err) {
      if (err instanceof ApiError && err.code === 'name_taken') {
        setNameError(`A script named “${name.trim()}” already exists. Choose another name.`)
      } else {
        setError(errorMessage(err))
      }
      setBusy(false)
    }
  }

  return (
    <Modal title="Save to library" onClose={onClose}>
      <form onSubmit={(e) => void submit(e)} noValidate>
        <div className="field">
          <label htmlFor={`${id}-name`}>Name</label>
          <input
            id={`${id}-name`}
            type="text"
            data-autofocus
            autoComplete="off"
            value={name}
            onChange={(e) => setName(e.target.value)}
            aria-invalid={nameError ? true : undefined}
            aria-describedby={nameError ? `${id}-name-error` : undefined}
          />
          {nameError ? (
            <p id={`${id}-name-error`} className="error" role="alert">
              {nameError}
            </p>
          ) : null}
        </div>
        <p className="hint">
          Saves the {SHELL_LABELS[shell]} script as it is in the form. Timeout and “run as” are not part of a library
          script.
        </p>
        <ErrorNote message={error} />
        <div className="modal-actions">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy}>
            {busy ? 'Saving…' : 'Save'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
