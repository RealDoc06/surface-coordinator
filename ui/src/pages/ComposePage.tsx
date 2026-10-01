import { useMemo, useState } from 'react'
import { Link, useLocation, useNavigate } from 'react-router-dom'
import type { Job, TargetSpec } from '../api/types'
import { ScriptComposer } from '../components/ScriptComposer'
import { parseComposeRequest } from '../lib/targets'
import { useStore } from '../state/StoreProvider'
import { sortedDevices } from '../state/store'

export function ComposePage() {
  const { state, dispatch } = useStore()
  const navigate = useNavigate()
  const location = useLocation()
  const request = useMemo(() => parseComposeRequest(location.state), [location.state])
  const [targets, setTargets] = useState<TargetSpec | null>(request.targets ?? null)
  const devices = useMemo(() => sortedDevices(state), [state.devices])

  function onCreated(job: Job) {
    // Show it at once; the job.created event carries the same job.
    dispatch({ type: 'event', event: { type: 'job.created', job } })
    navigate(`/jobs/${encodeURIComponent(job.id)}`)
  }

  const selectedIds = targets?.type === 'devices' ? targets.device_ids : []

  return (
    <>
      <div className="page-head">
        <h1>Run script</h1>
      </div>
      {state.devicesLoaded ? (
        <ScriptComposer
          targets={targets}
          devices={devices}
          initial={request.payload}
          onCreated={onCreated}
          onCancel={() => navigate(-1)}
          targetActions={
            <>
              <Link to="/" state={{ selected: selectedIds }}>
                {selectedIds.length > 0 ? 'Change selection' : 'Select devices'}
              </Link>
              {targets?.type !== 'all' ? (
                <button type="button" className="link" onClick={() => setTargets({ type: 'all' })}>
                  Run on all devices instead
                </button>
              ) : null}
            </>
          }
        />
      ) : (
        <p className="muted">Loading devices&hellip;</p>
      )}
    </>
  )
}
