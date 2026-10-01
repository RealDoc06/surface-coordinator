import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import { ErrorNote, StateBadge, StatusDot } from '../components/bits'
import { NetworkCell } from '../components/DeviceList'
import { ConfirmDialog } from '../components/Modal'
import { formatDateTime, relativeTime, useNow } from '../lib/format'
import { jobKindLabel, jobPreview } from '../lib/jobKinds'
import type { ComposeRequest } from '../lib/targets'
import { useStore } from '../state/StoreProvider'
import { keyLabels } from '../state/store'

export function DeviceDetailPage() {
  const { id } = useParams()
  if (id === undefined) return null
  return <DeviceDetail key={id} id={id} />
}

type Status = 'loading' | 'ready' | 'missing'

function DeviceDetail({ id }: { id: string }) {
  const { state, dispatch, reload } = useStore()
  const navigate = useNavigate()
  const now = useNow(15_000)
  const [status, setStatus] = useState<Status>('loading')
  const [error, setError] = useState<string | null>(null)
  const [confirming, setConfirming] = useState(false)
  const [forgetting, setForgetting] = useState(false)
  const [forgetError, setForgetError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    api.getDevice(id).then(
      ({ device, targets }) => {
        if (cancelled) return
        dispatch({ type: 'device.detail.loaded', device, targets })
        setStatus('ready')
        setError(null)
      },
      (err: unknown) => {
        if (cancelled) return
        if (err instanceof ApiError && err.status === 404) setStatus('missing')
        else setError(errorMessage(err))
      },
    )
    return () => {
      cancelled = true
    }
  }, [dispatch, id, state.syncEpoch])

  useEffect(() => () => dispatch({ type: 'device.detail.released', deviceId: id }), [dispatch, id])

  const device = state.devices[id]
  const targets = state.deviceTargets[id]

  async function forget() {
    setForgetting(true)
    setForgetError(null)
    try {
      await api.forgetDevice(id)
      dispatch({ type: 'event', event: { type: 'device.removed', device_id: id } })
      navigate('/')
    } catch (err) {
      setForgetError(errorMessage(err))
      setForgetting(false)
    }
  }

  if (status === 'missing' || (status === 'ready' && !device)) {
    return (
      <div className="empty">
        <h2>Device not found</h2>
        <p>
          It may have been forgotten. <Link to="/">Back to devices</Link>
        </p>
      </div>
    )
  }
  if (!device) {
    return error ? <ErrorNote message={error} onRetry={reload} /> : <p className="muted">Loading device&hellip;</p>
  }

  const runRequest: ComposeRequest = { targets: { type: 'devices', device_ids: [id] } }
  const keyLabel = keyLabels(state)[device.enrollment_key_id]

  return (
    <>
      <div className="page-head">
        <h1>{device.hostname || device.id}</h1>
        <span className="online-state">
          <StatusDot online={device.online} /> {device.online ? 'Online' : 'Offline'}
        </span>
        <span className="toolbar-spacer" />
        <button type="button" className="primary" onClick={() => navigate('/run', { state: runRequest })}>
          Run script on this device
        </button>
        <button type="button" onClick={() => navigate('/wifi', { state: runRequest })}>
          Move to Wi-Fi&hellip;
        </button>
        <button type="button" className="danger" onClick={() => setConfirming(true)}>
          Forget device
        </button>
      </div>
      <ErrorNote message={error} onRetry={reload} />

      <section className="panel">
        <dl className="fields">
          <div>
            <dt>Device id</dt>
            <dd className="mono">{device.id}</dd>
          </div>
          <div>
            <dt>OS</dt>
            <dd>{device.os}</dd>
          </div>
          <div>
            <dt>Client version</dt>
            <dd>{device.client_version}</dd>
          </div>
          <div>
            <dt>Network</dt>
            <dd>
              <NetworkCell network={device.network} />
            </dd>
          </div>
          <div>
            <dt>Enrollment key</dt>
            <dd>{keyLabel ?? <span className="mono">{device.enrollment_key_id}</span>}</dd>
          </div>
          <div>
            <dt>Last seen</dt>
            <dd title={formatDateTime(device.last_seen_at)}>
              {device.online ? 'now' : relativeTime(device.last_seen_at, now)}
            </dd>
          </div>
          <div>
            <dt>First seen</dt>
            <dd>{formatDateTime(device.first_seen_at)}</dd>
          </div>
        </dl>
      </section>

      <h2>Recent jobs</h2>
      {!targets ? (
        <p className="muted">Loading jobs&hellip;</p>
      ) : targets.length === 0 ? (
        <p className="muted">No jobs have been sent to this device.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Created</th>
              <th>Kind</th>
              <th>Job</th>
              <th>State</th>
              <th>Exit code</th>
            </tr>
          </thead>
          <tbody>
            {targets.map((target) => {
              const job = state.jobs[target.job.id]
              return (
                <tr key={target.id}>
                  <td className="nowrap" title={formatDateTime(target.job.created_at)}>
                    {relativeTime(target.job.created_at, now)}
                  </td>
                  <td className="nowrap">{jobKindLabel(target.job.kind)}</td>
                  <td className="col-preview">
                    <Link className="mono preview" to={`/jobs/${encodeURIComponent(target.job.id)}`}>
                      {(job && jobPreview(job)) || 'Open job'}
                    </Link>
                  </td>
                  <td>
                    <StateBadge state={target.state} />
                  </td>
                  <td className="mono">{target.exit_code ?? ''}</td>
                </tr>
              )
            })}
          </tbody>
        </table>
      )}

      {confirming ? (
        <ConfirmDialog
          title={`Forget ${device.hostname || 'this device'}?`}
          confirmLabel="Forget device"
          danger
          busy={forgetting}
          error={forgetError}
          onConfirm={() => void forget()}
          onCancel={() => {
            setConfirming(false)
            setForgetError(null)
          }}
        >
          <p>
            The device and its job history are removed and its connection is closed. If its client is still running
            with a valid enrollment key, it will reconnect and appear again as a new device.
          </p>
        </ConfirmDialog>
      ) : null}
    </>
  )
}
