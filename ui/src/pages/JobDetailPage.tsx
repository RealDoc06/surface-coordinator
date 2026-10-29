import { memo, useCallback, useEffect, useMemo, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import type { Device, Target } from '../api/types'
import { ErrorNote, StateBadge, StateCounts } from '../components/bits'
import { ConfirmDialog } from '../components/Modal'
import { OutputPane } from '../components/OutputPane'
import { formatDateTime, formatDuration, plural, shortId, useNow } from '../lib/format'
import { JobPayload, jobKindLabel, rerunRequest } from '../lib/jobKinds'
import { isCancelled, reasonText } from '../lib/reasons'
import { useStore } from '../state/StoreProvider'
import { isTerminal } from '../state/store'

function duration(target: Target, now: number): string {
  if (target.started_at === null) return ''
  if (target.finished_at !== null) return formatDuration(target.finished_at - target.started_at)
  return target.state === 'running' ? formatDuration(now - target.started_at) : ''
}

function outputPlaceholder(target: Target, loaded: boolean): string {
  if (!loaded) return 'Loading output…'
  if (isCancelled(target)) return 'Cancelled. The device produced no output.'
  switch (target.state) {
    case 'queued':
      return 'Waiting for the device to connect.'
    case 'sent':
      return 'Sent to the device; waiting for it to start.'
    case 'running':
      return 'Running; no output yet.'
    case 'expired':
      return 'The job expired before the device connected.'
    default:
      return 'No output.'
  }
}

export function JobDetailPage() {
  const { id } = useParams()
  if (id === undefined) return null
  // Keyed so that going from one job to another starts from a clean slate.
  return <JobDetail key={id} id={id} />
}

type Status = 'loading' | 'ready' | 'missing'
const hostnameOrder = new Intl.Collator(undefined, { numeric: true })

// Output chunks change frequently; the fleet table only needs device/state changes
// and its own one-second duration clock.
const TargetTable = memo(function TargetTable({ targets, devices, selectedId, onSelect }: {
  targets: Target[]
  devices: Record<string, Device>
  selectedId: string | undefined
  onSelect: (id: string) => void
}) {
  const now = useNow(1000)
  return (
    <section aria-label="Devices">
      <table className="table">
        <thead>
          <tr><th>Device</th><th>State</th><th>Exit code</th><th>Duration</th><th>Detail</th></tr>
        </thead>
        <tbody>
          {targets.map((target) => (
            <tr key={target.id} className={target.id === selectedId ? 'selected' : undefined}>
              <td>
                <button type="button" className="link" aria-pressed={target.id === selectedId} onClick={() => onSelect(target.id)}>
                  {devices[target.device_id]?.hostname || shortId(target.device_id)}
                </button>
              </td>
              <td><StateBadge state={target.state} reason={target.reason} /></td>
              <td className="mono">{target.exit_code ?? ''}</td>
              <td className="nowrap">{duration(target, now)}</td>
              <td>{reasonText(target)}</td>
            </tr>
          ))}
          {targets.length === 0 ? <tr><td colSpan={5} className="table-empty">This job has no devices left.</td></tr> : null}
        </tbody>
      </table>
    </section>
  )
})

function JobDetail({ id }: { id: string }) {
  const { state, dispatch, reload } = useStore()
  const navigate = useNavigate()
  const [status, setStatus] = useState<Status>('loading')
  const [error, setError] = useState<string | null>(null)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [cancelling, setCancelling] = useState(false)
  const [cancelBusy, setCancelBusy] = useState(false)
  const [cancelError, setCancelError] = useState<string | null>(null)
  const [outputError, setOutputError] = useState<{ targetId: string; message: string } | null>(null)
  const [outputRetry, setOutputRetry] = useState(0)

  const load = useCallback(async () => {
    try {
      const { job } = await api.getJob(id)
      dispatch({ type: 'job.detail.loaded', job })
      setStatus('ready')
      setError(null)
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) setStatus('missing')
      else setError(errorMessage(err))
    }
  }, [dispatch, id])

  useEffect(() => {
    void load()
  }, [load, state.syncEpoch])

  useEffect(() => () => dispatch({ type: 'job.detail.released', jobId: id }), [dispatch, id])

  const job = state.jobs[id]
  const context = useMemo(() => ({ settings: state.catalogue }), [state.catalogue])
  const hostname = useCallback((deviceId: string) => state.devices[deviceId]?.hostname || shortId(deviceId), [state.devices])
  const targets = useMemo(() => job?.targets.slice().sort((a, b) =>
    hostnameOrder.compare(hostname(a.device_id), hostname(b.device_id))) ?? [], [job?.targets, hostname])
  const selected = targets.find((t) => t.id === selectedId) ?? targets[0]
  const targetId = selected?.id
  const finished = selected ? isTerminal(selected.state) : false

  // A snapshot for one device, reconciled with events that cross the request.
  // Completing other targets never reloads output. Reconnects and this target's
  // completion read back stored output to recover missed or legacy events.
  useEffect(() => {
    if (!targetId) return
    let cancelled = false
    setOutputError(null)
    dispatch({ type: 'job.output.requested', jobId: id, targetId })
    api.getJobOutput(id, targetId).then(
      ({ output, output_truncated }) => {
        if (cancelled) return
        dispatch({ type: 'job.output.loaded', jobId: id, targetId, output, truncated: output_truncated })
      },
      (err: unknown) => {
        if (cancelled) return
        dispatch({ type: 'job.output.failed', jobId: id, targetId })
        setOutputError({ targetId, message: errorMessage(err) })
      },
    )
    return () => {
      cancelled = true
      dispatch({ type: 'job.output.failed', jobId: id, targetId })
    }
  }, [dispatch, id, targetId, finished, state.syncEpoch, outputRetry])

  // Also when the job was there and is gone: its last device was forgotten.
  if (status === 'missing' || (status === 'ready' && !job)) {
    return (
      <div className="empty">
        <h2>Job not found</h2>
        <p>
          It may have belonged to a device that was forgotten. <Link to="/jobs">Back to jobs</Link>
        </p>
      </div>
    )
  }
  if (!job) {
    return error ? <ErrorNote message={error} onRetry={reload} /> : <p className="muted">Loading job&hellip;</p>
  }

  const output = selected ? state.outputs[selected.id] : undefined
  const rerun = rerunRequest(job, context)
  const active = job.targets.filter((t) => !isTerminal(t.state)).length

  async function cancelJob() {
    setCancelBusy(true)
    setCancelError(null)
    try {
      const { job: updated } = await api.cancelJob(id)
      // Each change is also announced as an event; apply them now so the page is current.
      for (const target of updated.targets) {
        dispatch({ type: 'event', event: { type: 'job.target.updated', job_id: id, target } })
      }
      setCancelling(false)
      void load()
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) setStatus('missing')
      setCancelError(errorMessage(err))
    } finally {
      setCancelBusy(false)
    }
  }

  return (
    <>
      <div className="page-head">
        <h1>{jobKindLabel(job.kind)} job</h1>
        <span className="muted">
          {formatDateTime(job.created_at)}
          {job.created_by ? ` by ${job.created_by}` : ''}
        </span>
        <span className="toolbar-spacer" />
        <StateCounts targets={job.targets} />
        {active > 0 ? (
          <button type="button" className="danger" onClick={() => setCancelling(true)}>
            Cancel job
          </button>
        ) : null}
        {rerun ? (
          <button type="button" onClick={() => navigate(rerun.path, { state: rerun.state })}>
            Run again
          </button>
        ) : null}
      </div>
      <ErrorNote message={error} onRetry={reload} />

      <section className="panel">
        <JobPayload job={job} context={context} />
      </section>

      <div className="split">
        <TargetTable targets={targets} devices={state.devices} selectedId={selected?.id} onSelect={setSelectedId} />

        {selected ? (
          <section aria-label="Output">
            <div className="output-head">
              <h2>
                Output:{' '}
                {state.devices[selected.device_id] ? (
                  <Link to={`/devices/${encodeURIComponent(selected.device_id)}`}>{hostname(selected.device_id)}</Link>
                ) : (
                  hostname(selected.device_id)
                )}
              </h2>
              <StateBadge state={selected.state} reason={selected.reason} />
            </div>
            <ErrorNote message={outputError?.targetId === selected.id ? outputError.message : null} onRetry={() => setOutputRetry((n) => n + 1)} />
            <OutputPane
              streamKey={selected.id}
              label={`Output of ${hostname(selected.device_id)}`}
              text={output?.text ?? ''}
              truncated={output?.truncated ?? selected.output_truncated}
              placeholder={outputError?.targetId === selected.id && !output ? 'Output could not be loaded.' : outputPlaceholder(selected, output !== undefined)}
            />
          </section>
        ) : null}
      </div>

      {cancelling ? (
        <ConfirmDialog
          title="Cancel this job?"
          confirmLabel="Cancel job"
          danger
          busy={cancelBusy}
          error={cancelError}
          onConfirm={() => void cancelJob()}
          onCancel={() => {
            setCancelling(false)
            setCancelError(null)
          }}
        >
          <p>
            {plural(active, 'device')} {active === 1 ? 'has' : 'have'} not finished. Devices that have not started yet
            will not run it; the ones that are running are told to stop (a device that is offline is marked cancelled
            at once). Devices that already finished keep their result.
          </p>
        </ConfirmDialog>
      ) : null}
    </>
  )
}
