import { useState, type FormEvent } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import type { Group } from '../api/types'
import { ErrorNote } from '../components/bits'
import { GroupNameDialog } from '../components/GroupNameDialog'
import { ConfirmDialog } from '../components/Modal'
import { runOnGroup, validateGroupName } from '../lib/groups'
import { plural } from '../lib/format'
import { useStore } from '../state/StoreProvider'

export function GroupsPage() {
  const { state, reloadGroups } = useStore()
  const navigate = useNavigate()
  const [name, setName] = useState('')
  const [createError, setCreateError] = useState<string | null>(null)
  const [creating, setCreating] = useState(false)
  const [renaming, setRenaming] = useState<Group | null>(null)
  const [deleting, setDeleting] = useState<Group | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  async function onCreate(event: FormEvent) {
    event.preventDefault()
    if (creating) return
    const problem = validateGroupName(name)
    setCreateError(problem)
    if (problem) return
    setCreating(true)
    try {
      await api.createGroup(name.trim())
      setName('')
      reloadGroups()
    } catch (err) {
      setCreateError(
        err instanceof ApiError && err.code === 'name_taken'
          ? `A group named “${name.trim()}” already exists. Choose another name.`
          : errorMessage(err),
      )
    } finally {
      setCreating(false)
    }
  }

  async function onDelete() {
    if (!deleting) return
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      await api.deleteGroup(deleting.id)
      setDeleting(null)
      reloadGroups()
    } catch (err) {
      // Already gone elsewhere: nothing left to delete.
      if (err instanceof ApiError && err.status === 404) {
        setDeleting(null)
        reloadGroups()
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
        <h1>Groups</h1>
        {state.groupsLoaded ? <span className="muted">{plural(state.groups.length, 'group')}</span> : null}
      </div>
      <p className="lead">
        A group is a named set of devices. Run a job on a group and it goes to the members at that moment; change the
        membership on the group&rsquo;s page or from the device list.
      </p>

      <section className="panel" aria-labelledby="new-group">
        <h2 id="new-group">New group</h2>
        <form className="inline-form" onSubmit={(e) => void onCreate(e)} noValidate>
          <div className="field">
            <label htmlFor="new-group-name">Name</label>
            <input
              id="new-group-name"
              type="text"
              autoComplete="off"
              value={name}
              onChange={(e) => setName(e.target.value)}
              aria-invalid={createError ? true : undefined}
            />
          </div>
          <button type="submit" className="primary" disabled={creating}>
            Create group
          </button>
        </form>
        <ErrorNote message={createError} />
      </section>

      {!state.groupsLoaded ? (
        <p className="muted">Loading groups&hellip;</p>
      ) : state.groups.length === 0 ? (
        <div className="empty">
          <h2>No groups yet</h2>
          <p>Create one above, then add devices to it here or from the device list.</p>
        </div>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Devices</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {state.groups.map((group) => (
              <tr key={group.id}>
                <td>
                  <Link to={`/groups/${encodeURIComponent(group.id)}`}>{group.name}</Link>
                </td>
                <td>{group.device_ids.length}</td>
                <td className="row-actions">
                  <button
                    type="button"
                    aria-label={`Run on group ${group.name}`}
                    onClick={() => {
                      const run = runOnGroup(group.id)
                      navigate(run.path, { state: run.state })
                    }}
                  >
                    Run on group&hellip;
                  </button>
                  <button type="button" aria-label={`Rename ${group.name}`} onClick={() => setRenaming(group)}>
                    Rename
                  </button>
                  <button
                    type="button"
                    className="danger"
                    aria-label={`Delete ${group.name}`}
                    onClick={() => setDeleting(group)}
                  >
                    Delete
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {renaming ? (
        <GroupNameDialog
          group={renaming}
          onClose={() => setRenaming(null)}
          onRenamed={() => {
            setRenaming(null)
            reloadGroups()
          }}
        />
      ) : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete group “${deleting.name}”?`}
          confirmLabel="Delete group"
          danger
          busy={deleteBusy}
          error={deleteError}
          onConfirm={() => void onDelete()}
          onCancel={() => {
            setDeleting(null)
            setDeleteError(null)
          }}
        >
          <p>
            The group is removed; its {plural(deleting.device_ids.length, 'device')} stay enrolled. Jobs that already
            ran on the group are not affected.
          </p>
        </ConfirmDialog>
      ) : null}
    </>
  )
}
