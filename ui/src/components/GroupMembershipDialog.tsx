import { useId, useState } from 'react'
import { errorMessage } from '../api/client'
import type { Group } from '../api/types'
import { plural } from '../lib/format'
import { Modal } from './Modal'

export type MembershipOp = 'add' | 'remove'

type Props = {
  op: MembershipOp
  groups: Group[]
  /** The selected devices. */
  deviceIds: string[]
  onApply: (op: MembershipOp, groupId: string, deviceIds: string[]) => Promise<void>
  onClose: () => void
}

/** Add the selected devices to a group, or remove them from one. */
export function GroupMembershipDialog({ op, groups, deviceIds, onApply, onClose }: Props) {
  const selected = new Set(deviceIds)
  // Removing only makes sense from groups that have at least one of the devices.
  const choices =
    op === 'remove' ? groups.filter((g) => g.device_ids.some((id) => selected.has(id))) : groups
  const [groupId, setGroupId] = useState(choices[0]?.id ?? '')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const id = useId()

  const group = choices.find((g) => g.id === groupId)
  const affected = group
    ? deviceIds.filter((d) => (op === 'add' ? !group.device_ids.includes(d) : group.device_ids.includes(d))).length
    : 0

  async function apply() {
    if (!group || busy) return
    setBusy(true)
    setError(null)
    try {
      await onApply(op, group.id, deviceIds)
      onClose()
    } catch (err) {
      setError(errorMessage(err))
      setBusy(false)
    }
  }

  const title = op === 'add' ? 'Add to group' : 'Remove from group'

  return (
    <Modal title={title} onClose={onClose}>
      {choices.length === 0 ? (
        <p className="muted">
          {groups.length === 0
            ? 'There are no groups yet. Create one on the Groups page.'
            : 'None of the selected devices is in a group.'}
        </p>
      ) : (
        <>
          <div className="field">
            <label htmlFor={`${id}-group`}>Group</label>
            <select id={`${id}-group`} data-autofocus value={groupId} onChange={(e) => setGroupId(e.target.value)}>
              {choices.map((g) => (
                <option key={g.id} value={g.id}>
                  {g.name} ({plural(g.device_ids.length, 'member')})
                </option>
              ))}
            </select>
          </div>
          <p>
            {op === 'add'
              ? affected === 0
                ? 'All selected devices are already in this group.'
                : `${plural(affected, 'device')} will be added to “${group?.name}”.`
              : `${plural(affected, 'device')} will be removed from “${group?.name}”.`}
            {affected < deviceIds.length && affected > 0 ? (
              <span className="muted"> The others are {op === 'add' ? 'already' : 'not'} in it.</span>
            ) : null}
          </p>
        </>
      )}
      {error ? (
        <p className="error" role="alert">
          {error}
        </p>
      ) : null}
      <div className="modal-actions">
        <button type="button" onClick={onClose}>
          Cancel
        </button>
        {choices.length > 0 ? (
          <button type="button" className="primary" disabled={busy || affected === 0} onClick={() => void apply()}>
            {op === 'add' ? 'Add' : 'Remove'}
          </button>
        ) : null}
      </div>
    </Modal>
  )
}
