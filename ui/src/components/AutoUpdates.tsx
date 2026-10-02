import { useId, useState } from 'react'
import { Link } from 'react-router-dom'
import { api, errorMessage } from '../api/client'
import type { Rollout, RolloutState, Updates } from '../api/types'
import { useStore } from '../state/StoreProvider'
import { ErrorNote } from './bits'

/** The switch for automatic, staged client updates. */
export function AutoUpdateSwitch({ enabled, onChanged }: { enabled: boolean; onChanged: (updates: Updates) => void }) {
  const id = useId()
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function change(next: boolean) {
    setSaving(true)
    setError(null)
    try {
      onChanged(await api.setAutoUpdate(next))
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <>
      <label className="switch">
        <input
          type="checkbox"
          role="switch"
          checked={enabled}
          disabled={saving}
          aria-describedby={`${id}-help`}
          onChange={(e) => void change(e.target.checked)}
        />
        <span className="switch-track" aria-hidden="true" />
        <span>Update devices automatically</span>
      </label>
      <p className="hint" id={`${id}-help`}>
        One device is updated first; the others follow once it is back on the new version. If that update fails,
        nothing else is updated until a device has been updated successfully by hand.
      </p>
      <ErrorNote message={error} />
    </>
  )
}

const ROLLOUT_STATES: Record<RolloutState, { label: string; badge: string }> = {
  canary: { label: 'Testing on one device', badge: 'badge-info' },
  rolling: { label: 'Rolling out', badge: 'state-running' },
  halted: { label: 'Halted', badge: 'state-failed' },
  done: { label: 'All devices updated', badge: 'state-succeeded' },
}

/** Where the staged rollout of the newest version stands. */
export function RolloutStatus({ rollout }: { rollout: Rollout }) {
  const { state } = useStore()
  // A state this UI does not know yet (newer coordinator) is shown as it is.
  const shown: (typeof ROLLOUT_STATES)[RolloutState] | undefined = ROLLOUT_STATES[rollout.state]
  const canaryId = rollout.canary_device_id
  const canary = canaryId ? state.devices[canaryId] : undefined

  return (
    <>
      <p>
        <strong>{rollout.version}</strong>{' '}
        <span className={`badge ${shown?.badge ?? ''}`}>{shown?.label ?? rollout.state}</span>
      </p>
      {canaryId ? (
        <p>
          <span className="muted">First device:</span>{' '}
          <Link to={`/devices/${encodeURIComponent(canaryId)}`}>{canary?.hostname ?? canaryId}</Link>
        </p>
      ) : null}
      <p className="state-counts">
        <span className="count state-succeeded">{rollout.updated} updated</span>
        <span className="count state-running">{rollout.pending} pending</span>
        <span className="count state-failed">{rollout.failed} failed</span>
      </p>
      {rollout.state === 'halted' ? (
        <p className="notice" role="note">
          The rollout stopped because an update failed. Update one device by hand from the device list; if that
          succeeds, the rest follow.
        </p>
      ) : null}
    </>
  )
}
