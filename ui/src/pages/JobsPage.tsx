import { useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { api, errorMessage, JOBS_PAGE_SIZE } from '../api/client'
import { ErrorNote, StateCounts } from '../components/bits'
import { formatDateTime, relativeTime, useNow } from '../lib/format'
import { jobKindLabel, jobPreview } from '../lib/jobKinds'
import { useStore } from '../state/StoreProvider'
import { listedJobs } from '../state/store'

export function JobsPage() {
  const { state, dispatch, reload } = useStore()
  const [error, setError] = useState<string | null>(null)
  const [loadingMore, setLoadingMore] = useState(false)
  const now = useNow(15_000)

  useEffect(() => {
    let cancelled = false
    api.listJobs({ limit: JOBS_PAGE_SIZE }).then(
      ({ jobs }) => {
        if (cancelled) return
        dispatch({ type: 'jobs.loaded', jobs, pageSize: JOBS_PAGE_SIZE, append: false })
        setError(null)
      },
      (err: unknown) => {
        if (!cancelled) setError(errorMessage(err))
      },
    )
    return () => {
      cancelled = true
    }
  }, [dispatch, state.syncEpoch])

  const jobs = useMemo(() => listedJobs(state), [state.jobs, state.jobList])
  const { loaded, cursor, hasMore } = state.jobList

  async function loadMore() {
    if (cursor === null || loadingMore) return
    setLoadingMore(true)
    try {
      const { jobs: page } = await api.listJobs({ limit: JOBS_PAGE_SIZE, before: cursor })
      dispatch({ type: 'jobs.loaded', jobs: page, pageSize: JOBS_PAGE_SIZE, append: true })
      setError(null)
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setLoadingMore(false)
    }
  }

  return (
    <>
      <div className="page-head">
        <h1>Jobs</h1>
      </div>
      <ErrorNote message={error} onRetry={reload} />
      {!loaded ? (
        error ? null : <p className="muted">Loading jobs&hellip;</p>
      ) : jobs.length === 0 ? (
        <div className="empty">
          <h2>No jobs yet</h2>
          <p>
            Select devices on the <Link to="/">Devices</Link> page and choose &ldquo;Run script&hellip;&rdquo; to create one.
          </p>
        </div>
      ) : (
        <>
          <table className="table">
            <thead>
              <tr>
                <th>Created</th>
                <th>Kind</th>
                <th>Job</th>
                <th>Created by</th>
                <th>Devices</th>
              </tr>
            </thead>
            <tbody>
              {jobs.map((job) => (
                <tr key={job.id}>
                  <td className="nowrap" title={formatDateTime(job.created_at)}>
                    {relativeTime(job.created_at, now)}
                  </td>
                  <td className="nowrap">{jobKindLabel(job.kind)}</td>
                  <td className="col-preview">
                    <Link className="mono preview" to={`/jobs/${encodeURIComponent(job.id)}`}>
                      {jobPreview(job) || '(empty)'}
                    </Link>
                  </td>
                  <td className="nowrap">{job.created_by ?? <span className="muted">&mdash;</span>}</td>
                  <td className="nowrap">
                    <StateCounts targets={job.targets} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {hasMore ? (
            <p className="more">
              <button type="button" onClick={() => void loadMore()} disabled={loadingMore}>
                {loadingMore ? 'Loading…' : 'Load more'}
              </button>
            </p>
          ) : null}
        </>
      )}
    </>
  )
}
