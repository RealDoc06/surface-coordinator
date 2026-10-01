import { useMemo, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import { ErrorNote } from '../components/bits'
import { DeviceChecklist } from '../components/DeviceChecklist'
import { GroupNameDialog } from '../components/GroupNameDialog'
import { ConfirmDialog } from '../components/Modal'
import { plural } from '../lib/format'
import { runOnGroup } from '../lib/groups'
import { useStore } from '../state/StoreProvider'
import { sortedDevices } from '../state/store'

export function GroupPage() {
  const { id } = useParams()
  if (id === undefined) return null
  return <GroupDetail key={id} id={id} />
}

function sameSet(a: ReadonlySet<string>, b: ReadonlySet<string>): boolean {
  return a.size === b.size && [...a].every((x) => b.has(x))
}

function GroupDetail({ id }: { id: string }) {
  const { state, reloadGroups } = useStore()
  const navigate = useNavigate()
  const group = state.groups.find((g) => g.id === id)
  const devices = useMemo(() => sortedDevices(state), [state.devices])

  /** The membership being edited; null until it is taken from the group. */
  const [draft, setDraft] = useState<Set<string> | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)
  const [renaming, setRenaming] = useState(false)
  const [deleting, setDeleting] = useState(false)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  if (!state.groupsLoaded) return <p className="muted">Loading group&hellip;</p>
  if (!group) {
    return (
      <div className="empty">
        <h2>Group not found</h2>
        <p>
          It may have been deleted. <Link to="/groups">Back to groups</Link>
        </p>
      </div>
    )
  }

  const stored = new Set(group.device_ids)
  const editing = draft ?? stored
  const dirty = !sameSet(editing, stored)
  // Only members that still exist as devices can be edited.
  const known = new Set(devices.map((d) => d.id))

  async function save() {
    if (!group || saving) return
    setSaving(true)
    setError(null)
    setSaved(false)
    try {
      await api.setGroupMembers(group.id, [...editing].filter((d) => known.has(d)))
      setDraft(null)
      setSaved(true)
      reloadGroups()
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  async function onDelete() {
    if (!group) return
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      await api.deleteGroup(group.id)
      reloadGroups()
      navigate('/groups')
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) {
        reloadGroups()
        navigate('/groups')
      } else {
        setDeleteError(errorMessage(err))
        setDeleteBusy(false)
      }
    }
  }

  return (
    <>
      <div className="page-head">
        <h1>{group.name}</h1>
        <span className="muted">{plural(group.device_ids.length, 'device')}</span>
        <span className="toolbar-spacer" />
        <button
          type="button"
          className="primary"
          onClick={() => {
            const run = runOnGroup(group.id)
            navigate(run.path, { state: run.state })
          }}
        >
          Run on group&hellip;
        </button>
        <button type="button" onClick={() => setRenaming(true)}>
          Rename&hellip;
        </button>
        <button type="button" className="danger" onClick={() => setDeleting(true)}>
          Delete group
        </button>
      </div>
      <p className="muted">
        <Link to="/groups">All groups</Link>
      </p>

      <section className="panel" aria-labelledby="members">
        <h2 id="members">Members</h2>
        <DeviceChecklist
          devices={devices}
          selected={editing}
          label={`Members of ${group.name}`}
          onChange={(next) => {
            setDraft(next)
            setSaved(false)
          }}
        />
        <ErrorNote message={error} />
        <div className="form-actions">
          <button type="button" className="primary" disabled={!dirty || saving} onClick={() => void save()}>
            {saving ? 'Saving…' : 'Save members'}
          </button>
          <button type="button" disabled={!dirty || saving} onClick={() => setDraft(null)}>
            Revert
          </button>
          {saved && !dirty ? (
            <span className="success" role="status">
              Saved.
            </span>
          ) : null}
        </div>
      </section>

      {renaming ? (
        <GroupNameDialog
          group={group}
          onClose={() => setRenaming(false)}
          onRenamed={() => {
            setRenaming(false)
            reloadGroups()
          }}
        />
      ) : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete group “${group.name}”?`}
          confirmLabel="Delete group"
          danger
          busy={deleteBusy}
          error={deleteError}
          onConfirm={() => void onDelete()}
          onCancel={() => {
            setDeleting(false)
            setDeleteError(null)
          }}
        >
          <p>
            The group is removed; its {plural(group.device_ids.length, 'device')} stay enrolled. Jobs that already ran
            on the group are not affected.
          </p>
        </ConfirmDialog>
      ) : null}
    </>
  )
}
