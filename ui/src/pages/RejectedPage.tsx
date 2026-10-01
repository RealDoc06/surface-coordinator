import { useEffect } from 'react'
import { formatDateTime, shortId } from '../lib/format'
import { useStore } from '../state/StoreProvider'
import { keyLabels } from '../state/store'

export function RejectedPage() {
  const { state, dispatch } = useStore()

  // Looking at this page is what clears the badge, including for attempts
  // that arrive while it is open.
  useEffect(() => {
    if (state.rejectedUnseen > 0) dispatch({ type: 'rejected.seen' })
  }, [dispatch, state.rejectedUnseen])

  const labels = keyLabels(state)

  return (
    <>
      <div className="page-head">
        <h1>Rejected connections</h1>
      </div>
      <p className="lead">
        Clients that tried to connect and were turned away, newest first (the last 200). Usual causes are a wrong or
        revoked enrollment key, or a client that is too old.
      </p>
      {!state.rejectedLoaded ? (
        <p className="muted">Loading&hellip;</p>
      ) : state.rejected.length === 0 ? (
        <p className="muted">No rejected connections.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Time</th>
              <th>Peer</th>
              <th>Device</th>
              <th>Key</th>
              <th>Reason</th>
            </tr>
          </thead>
          <tbody>
            {state.rejected.map((r) => (
              <tr key={r.id}>
                <td className="nowrap">{formatDateTime(r.at)}</td>
                <td className="mono">{r.peer}</td>
                <td>
                  {r.hostname ?? <span className="muted">unknown host</span>}
                  {r.device_id ? (
                    <span className="muted mono" title={r.device_id}>
                      {' '}
                      {shortId(r.device_id)}
                    </span>
                  ) : null}
                </td>
                <td>
                  {r.key_id === null ? (
                    <span className="muted">&mdash;</span>
                  ) : (
                    (labels[r.key_id] ?? (
                      <span className="mono" title={r.key_id}>
                        {shortId(r.key_id)}
                      </span>
                    ))
                  )}
                </td>
                <td>{r.reason}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  )
}
