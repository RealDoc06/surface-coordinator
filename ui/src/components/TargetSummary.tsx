import type { Device, TargetSpec } from '../api/types'
import { plural, shortId } from '../lib/format'
import { resolveTargets } from '../lib/targets'

const MAX_NAMES = 8

type Props = {
  spec: TargetSpec
  /** Every device the coordinator knows. */
  devices: Device[]
}

/** Says, in words, which devices a job will go to. */
export function TargetSummary({ spec, devices }: Props) {
  const resolved = resolveTargets(spec, devices)
  const count = resolved.devices.length

  let headline: string
  switch (spec.type) {
    case 'all':
      headline = `All devices (${count})`
      break
    case 'devices': {
      const names = resolved.devices.map((d) => d.hostname || shortId(d.id))
      const listed = names.slice(0, MAX_NAMES).join(', ')
      const more = names.length > MAX_NAMES ? ` and ${names.length - MAX_NAMES} more` : ''
      headline = count === 0 ? 'No devices selected' : `${plural(count, 'selected device')}: ${listed}${more}`
      break
    }
  }

  return (
    <div className="target-summary">
      <p className="target-headline">{headline}</p>
      {resolved.offline > 0 ? (
        <p className="muted">
          {resolved.offline === count
            ? count === 1
              ? 'It is offline.'
              : `All ${count} are offline.`
            : `${resolved.offline} of them ${resolved.offline === 1 ? 'is' : 'are'} offline.`}{' '}
          Offline devices get the job when they reconnect.
        </p>
      ) : null}
      {resolved.missing.length > 0 ? (
        <p className="muted">
          {plural(resolved.missing.length, 'selected device')} no longer{' '}
          {resolved.missing.length === 1 ? 'exists' : 'exist'} and will be skipped.
        </p>
      ) : null}
    </div>
  )
}
