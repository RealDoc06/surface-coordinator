import { useId, useState, type ReactNode } from 'react'
import type { Device, Group, Job, PowerAction, TargetSpec } from '../api/types'
import { plural } from '../lib/format'
import { POWER_LABELS } from '../lib/jobKinds'
import type { PowerPrefill } from '../lib/targets'
import { JobForm, type Confirmation } from './JobForm'

type Props = {
  targets: TargetSpec | null
  devices: Device[]
  groups?: Group[]
  initial?: PowerPrefill
  targetActions?: ReactNode
  onCreated: (job: Job) => void
  onCancel?: () => void
}

const ACTIONS: { action: PowerAction; help: string }[] = [
  { action: 'restart', help: 'The device restarts after a 5-second delay and reconnects by itself.' },
  {
    action: 'shutdown',
    help: 'The device turns off after a 5-second delay. Someone has to switch it on again at the device; it cannot be woken from here.',
  },
  { action: 'sleep', help: 'The device goes to sleep. The job stays running while it is asleep and completes after it wakes up.' },
]

const VERB: Record<PowerAction, string> = { restart: 'Restart', shutdown: 'Shut down', sleep: 'Put to sleep' }

/** What the confirmation says; names how many devices are affected. Exported for tests. */
export function powerConfirmation(action: PowerAction, count: number): Confirmation {
  const devices = plural(count, 'device')
  const label = POWER_LABELS[action]
  switch (action) {
    case 'restart':
      return {
        title: `Restart ${devices}?`,
        confirmLabel: label,
        body: (
          <p>
            Each device restarts after a 5-second delay, whoever is using it. Unsaved work on the device is lost. Offline devices
            restart when they reconnect.
          </p>
        ),
      }
    case 'shutdown':
      return {
        title: `Shut down ${devices}?`,
        confirmLabel: label,
        danger: true,
        body: (
          <p>
            Each device turns off after a 5-second delay, whoever is using it, and unsaved work is lost.{' '}
            <strong>Someone has to switch it on again at the device.</strong> Offline devices shut down when they
            reconnect.
          </p>
        ),
      }
    case 'sleep':
      return {
        title: `Put ${devices} to sleep?`,
        confirmLabel: label,
        body: <p>Each device goes to sleep and reconnects when it wakes up. The job stays running while devices are asleep and completes after they wake up.</p>,
      }
  }
}

/** The form that creates a power job. Submitting asks for confirmation. */
export function PowerForm({ targets, devices, groups, initial, targetActions, onCreated, onCancel }: Props) {
  const [action, setAction] = useState<PowerAction | null>(initial?.action ?? null)
  const [error, setError] = useState<string | null>(null)
  const ids = useId()

  return (
    <JobForm
      verb="act on"
      targets={targets}
      devices={devices}
      groups={groups}
      targetActions={targetActions}
      onCreated={onCreated}
      onCancel={onCancel}
      submitLabel={(n) =>
        action === null ? 'Continue…' : n > 0 ? `${VERB[action]} ${plural(n, 'device')}…` : `${VERB[action]}…`
      }
      confirm={(n) => powerConfirmation(action ?? 'restart', n)}
      build={() => {
        if (action === null) {
          setError('Choose restart, shut down or sleep.')
          return null
        }
        setError(null)
        return { kind: 'power', payload: { action } }
      }}
    >
      <section className="panel" aria-labelledby={`${ids}-power`}>
        <h2 id={`${ids}-power`}>Action</h2>
        <fieldset className="choice stacked" aria-describedby={error ? `${ids}-error` : undefined}>
          <legend className="visually-hidden">Power action</legend>
          {ACTIONS.map(({ action: value, help }) => (
            <label key={value} className={value === 'shutdown' ? 'power-option danger-text' : 'power-option'}>
              <input
                type="radio"
                name={`${ids}-action`}
                checked={action === value}
                onChange={() => {
                  setAction(value)
                  setError(null)
                }}
              />
              <span>
                <strong>{POWER_LABELS[value]}</strong>
                <span className="muted block">{help}</span>
              </span>
            </label>
          ))}
        </fieldset>
        {error ? (
          <p id={`${ids}-error`} className="error" role="alert">
            {error}
          </p>
        ) : null}
        <p className="hint">You are asked to confirm before anything is sent.</p>
      </section>
    </JobForm>
  )
}
