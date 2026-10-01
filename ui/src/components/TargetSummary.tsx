import type { Device, Group, TargetSpec } from '../api/types'
import { plural, shortId } from '../lib/format'
import { resolveTargets } from '../lib/targets'

const MAX_NAMES = 8

type Props = {
  spec: TargetSpec
  /** Every device the coordinator knows. */
  devices: Device[]
  /** Every group; needed to describe a group target. */
  groups?: Group[]
}

/** Says, in words, which devices a job will go to. */
export function TargetSummary({ spec, devices, groups = [] }: Props) {
  const resolved = resolveTargets(spec, devices, groups)
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
    case 'group':
      headline = resolved.group
        ? `Group “${resolved.group.name}” (${plural(count, 'device')} now)`
        : 'This group no longer exists'
      break
  }

  return (
    <div className="target-summary">
      <p className="target-headline">{headline}</p>
      {spec.type === 'group' && resolved.group ? (
        <p className="muted">
          Membership is taken at the moment the job is created, so devices added to or removed from the group
          before then are included or left out.
          {count === 0 ? ' The group has no devices at the moment.' : ''}
        </p>
      ) : null}
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
