import { useEffect, useId, useState, type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { api, errorMessage } from '../api/client'
import type { ClientRelease, Device, Group, Job, TargetSpec } from '../api/types'
import { formatBytes, formatDateTime, plural } from '../lib/format'
import { resolveTargets, type UpdatePrefill } from '../lib/targets'
import { compareVersions, isOlder } from '../lib/versions'
import { ErrorNote } from './bits'
import { JobForm } from './JobForm'

type Props = {
  targets: TargetSpec | null
  devices: Device[]
  groups?: Group[]
  initial?: UpdatePrefill
  targetActions?: ReactNode
  onCreated: (job: Job) => void
  onCancel?: () => void
}

/** How a chosen version relates to the versions of the devices it would go to. */
export function versionImpact(version: string, devices: Pick<Device, 'client_version'>[]) {
  let update = 0
  let same = 0
  let newer = 0
  for (const d of devices) {
    const cmp = compareVersions(d.client_version, version)
    if (cmp === null) update++
    else if (cmp < 0) update++
    else if (cmp === 0) same++
    else newer++
  }
  return { update, same, newer }
}

/** The form that creates a client.update job. */
export function ClientUpdateForm({ targets, devices, groups, initial, targetActions, onCreated, onCancel }: Props) {
  const [cached, setCached] = useState<ClientRelease[] | null>(null)
  const [latest, setLatest] = useState<string | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [version, setVersion] = useState<string | null>(null)
  const [downgrade, setDowngrade] = useState(initial?.downgrade === true)
  const [error, setError] = useState<string | null>(null)
  const ids = useId()

  useEffect(() => {
    let cancelled = false
    api.getUpdates().then(
      (updates) => {
        if (cancelled) return
        setCached(updates.cached)
        setLatest(updates.latest?.version ?? updates.cached[0]?.version ?? null)
        setLoadError(null)
      },
      (err: unknown) => {
        if (!cancelled) setLoadError(errorMessage(err))
      },
    )
    return () => {
      cancelled = true
    }
  }, [])

  const has = (v: string | undefined) => v !== undefined && (cached ?? []).some((r) => r.version === v)
  const chosen = version ?? (has(initial?.version) ? (initial?.version ?? null) : latest)
  const release = cached?.find((r) => r.version === chosen)

  const resolved = targets ? resolveTargets(targets, devices, groups) : null
  const targetDevices = resolved?.devices ?? []
  const needsDowngrade = chosen !== null && targetDevices.some((d) => isOlder(chosen, d.client_version))
  const impact = chosen !== null ? versionImpact(chosen, targetDevices) : null

  return (
    <JobForm
      targetsHeading="Devices to update"
      verb="update"
      acceptedByRestricted
      targets={targets}
      devices={devices}
      groups={groups}
      targetActions={targetActions}
      onCreated={onCreated}
      onCancel={onCancel}
      submitLabel={(n) => (chosen && n > 0 ? `Update ${plural(n, 'device')} to ${chosen}…` : 'Update…')}
      confirm={(n) => ({
        title: `Update ${plural(n, 'device')} to ${chosen ?? ''}?`,
        confirmLabel: 'Update',
        body: (
          <>
            <p>
              Each device downloads the update from this coordinator, checks its signature and checksum, installs it,
              restarts the client and reconnects. Offline devices update when they reconnect.
            </p>
            {needsDowngrade && downgrade ? (
              <p>
                <strong>Downgrade allowed:</strong> devices on a newer version are set back to {chosen}.
              </p>
            ) : null}
          </>
        ),
      })}
      build={() => {
        if (!chosen) {
          setError('There is no client version to install.')
          return null
        }
        setError(null)
        return {
          kind: 'client.update',
          payload: needsDowngrade && downgrade ? { version: chosen, downgrade: true } : { version: chosen },
        }
      }}
    >
      <section className="panel" aria-labelledby={`${ids}-version`}>
        <h2 id={`${ids}-version`}>Version</h2>
        <ErrorNote message={loadError} />
        {cached === null ? (
          loadError ? null : <p className="muted">Loading versions&hellip;</p>
        ) : cached.length === 0 ? (
          <p className="muted">
            No client version has been downloaded yet. Check for new versions on the <Link to="/updates">Updates</Link>{' '}
            page.
          </p>
        ) : (
          <>
            <div className="field">
              <label htmlFor={`${ids}-pick`}>Install version</label>
              <select
                id={`${ids}-pick`}
                value={chosen ?? ''}
                onChange={(e) => {
                  setVersion(e.target.value)
                  setError(null)
                }}
              >
                {cached.map((r) => (
                  <option key={r.version} value={r.version}>
                    {r.version}
                    {r.version === latest ? ' (newest)' : ''}
                  </option>
                ))}
              </select>
              {release ? (
                <p className="hint">
                  {formatBytes(release.size)}, downloaded {formatDateTime(release.cached_at)}
                </p>
              ) : null}
            </div>

            {impact && chosen !== null ? (
              <p>
                {impact.update > 0 ? <>{plural(impact.update, 'device')} will be updated to {chosen}. </> : null}
                {impact.same > 0 ? (
                  <span className="muted">
                    {plural(impact.same, 'device')} {impact.same === 1 ? 'is' : 'are'} already on {chosen} and{' '}
                    {impact.same === 1 ? 'is' : 'are'} left as {impact.same === 1 ? 'it is' : 'they are'}.{' '}
                  </span>
                ) : null}
                {impact.newer > 0 ? (
                  <span className="muted">
                    {plural(impact.newer, 'device')} {impact.newer === 1 ? 'runs' : 'run'} a newer version than {chosen}.
                  </span>
                ) : null}
                {impact.update + impact.same + impact.newer === 0 ? <span className="muted">No devices selected.</span> : null}
              </p>
            ) : null}

            {needsDowngrade ? (
              <label className="check-row">
                <input type="checkbox" checked={downgrade} onChange={(e) => setDowngrade(e.target.checked)} />
                <span>
                  Allow downgrade
                  <span className="muted block">
                    At least one selected device runs a newer version than {chosen}. Devices refuse to go back to an
                    older version unless this is ticked.
                  </span>
                </span>
              </label>
            ) : null}
          </>
        )}
        {error ? (
          <p className="error" role="alert">
            {error}
          </p>
        ) : null}
      </section>
    </JobForm>
  )
}
