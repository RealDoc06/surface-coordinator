import { useId, useState, type FormEvent } from 'react'
import { api, ApiError, errorMessage } from '../api/client'
import type { Group } from '../api/types'
import { validateGroupName } from '../lib/groups'
import { Modal } from './Modal'

/** Rename a group. A name that is taken is reported next to the field. */
export function GroupNameDialog({
  group,
  onRenamed,
  onClose,
}: {
  group: Group
  onRenamed: (group: Group) => void
  onClose: () => void
}) {
  const [name, setName] = useState(group.name)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const id = useId()

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (busy) return
    const problem = validateGroupName(name)
    setError(problem)
    if (problem) return
    if (name.trim() === group.name) {
      onClose()
      return
    }
    setBusy(true)
    try {
      const { group: renamed } = await api.renameGroup(group.id, name.trim())
      onRenamed(renamed)
    } catch (err) {
      setError(
        err instanceof ApiError && err.code === 'name_taken'
          ? `A group named “${name.trim()}” already exists. Choose another name.`
          : errorMessage(err),
      )
      setBusy(false)
    }
  }

  return (
    <Modal title="Rename group" onClose={onClose}>
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
            aria-invalid={error ? true : undefined}
          />
          {error ? (
            <p className="error" role="alert">
              {error}
            </p>
          ) : null}
        </div>
        <div className="modal-actions">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy}>
            Rename
          </button>
        </div>
      </form>
    </Modal>
  )
}
