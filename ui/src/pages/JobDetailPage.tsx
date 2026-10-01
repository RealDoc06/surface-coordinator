import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import type { Target } from '../api/types'
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

function JobDetail({ id }: { id: string }) {
  const { state, dispatch, reload } = useStore()
  const navigate = useNavigate()
  const now = useNow(1000)
  const [status, setStatus] = useState<Status>('loading')
  const [error, setError] = useState<string | null>(null)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [cancelling, setCancelling] = useState(false)
  const [cancelBusy, setCancelBusy] = useState(false)
  const [cancelError, setCancelError] = useState<string | null>(null)
  /** Finished targets as of the last load, to spot ones that finish while watching. */
  const finishedSeen = useRef(0)

  const load = useCallback(async () => {
    dispatch({ type: 'job.detail.requested', jobId: id })
    try {
      const { job } = await api.getJob(id)
      finishedSeen.current = Math.max(finishedSeen.current, job.targets.filter((t) => isTerminal(t.state)).length)
      dispatch({ type: 'job.detail.loaded', job })
      setStatus('ready')
      setError(null)
    } catch (err) {
      dispatch({ type: 'job.detail.failed', jobId: id })
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
  const finished = job ? job.targets.filter((t) => isTerminal(t.state)).length : 0

  // Events from a coordinator that sends no output offsets are placed by
  // content when they cross a load in flight, which can rarely misplace one.
  // When a device finishes, read back what the coordinator stored so the final
  // output is exactly that (with offsets this is only a cheap confirmation).
  useEffect(() => {
    if (status === 'ready' && finished > finishedSeen.current) {
      finishedSeen.current = finished
      void load()
    }
  }, [finished, status, load])

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

  const hostname = (deviceId: string) => state.devices[deviceId]?.hostname || shortId(deviceId)
  const targets = job.targets
    .slice()
    .sort((a, b) => hostname(a.device_id).localeCompare(hostname(b.device_id), undefined, { numeric: true }))
  const selected = targets.find((t) => t.id === selectedId) ?? targets[0]
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
        <section aria-label="Devices">
          <table className="table">
            <thead>
              <tr>
                <th>Device</th>
                <th>State</th>
                <th>Exit code</th>
                <th>Duration</th>
                <th>Detail</th>
              </tr>
            </thead>
            <tbody>
              {targets.map((target) => (
                <tr key={target.id} className={target.id === selected?.id ? 'selected' : undefined}>
                  <td>
                    <button
                      type="button"
                      className="link"
                      aria-pressed={target.id === selected?.id}
                      onClick={() => setSelectedId(target.id)}
                    >
                      {hostname(target.device_id)}
                    </button>
                  </td>
                  <td>
                    <StateBadge state={target.state} reason={target.reason} />
                  </td>
                  <td className="mono">{target.exit_code ?? ''}</td>
                  <td className="nowrap">{duration(target, now)}</td>
                  <td>{reasonText(target)}</td>
                </tr>
              ))}
              {targets.length === 0 ? (
                <tr>
                  <td colSpan={5} className="table-empty">
                    This job has no devices left.
                  </td>
                </tr>
              ) : null}
            </tbody>
          </table>
        </section>

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
            <OutputPane
              streamKey={selected.id}
              label={`Output of ${hostname(selected.device_id)}`}
              text={output?.text ?? ''}
              truncated={output?.truncated ?? selected.output_truncated}
              placeholder={outputPlaceholder(selected, output !== undefined)}
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
