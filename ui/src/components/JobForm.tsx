import { useId, useState, type FormEvent, type ReactNode } from 'react'
import { api, errorMessage } from '../api/client'
import type { CreateJob, Device, Group, Job, TargetSpec } from '../api/types'
import { plural } from '../lib/format'
import { noTargetsMessage, requestTargets, resolveTargets } from '../lib/targets'
import { ConfirmDialog } from './Modal'
import { TargetSummary } from './TargetSummary'

/** A job without its targets: what a form builds. */
export type JobBody = CreateJob extends infer J ? (J extends CreateJob ? Omit<J, 'targets'> : never) : never

export type Confirmation = {
  title: string
  body: ReactNode
  confirmLabel: string
  danger?: boolean
}

type Props = {
  /** Heading of the targets panel. */
  targetsHeading?: string
  /** Completes "There are no devices to ..." when nothing is targeted. */
  verb: string
  /** Where the job goes; null when nothing has been chosen yet. */
  targets: TargetSpec | null
  /** Every device the coordinator knows. */
  devices: Device[]
  groups?: Group[]
  /** Ways to change the targets, supplied by the page. */
  targetActions?: ReactNode
  /**
   * Checks the fields (showing their errors) and returns what to send, or null
   * when something is wrong. Called on submit.
   */
  build: () => JobBody | null
  /** Text of the submit button, given the number of devices it would go to. */
  submitLabel: (deviceCount: number) => string
  /** When given, submitting asks first; the job is only created on confirmation. */
  confirm?: (deviceCount: number) => Confirmation
  /** True for client.update, the one job a restricted device accepts. */
  acceptedByRestricted?: boolean
  onCreated: (job: Job) => void
  onCancel?: () => void
  /** The form's own panels. */
  children: ReactNode
}

/**
 * The frame every job form shares: the targets panel, validation of the
 * targets, the optional confirmation, creating the job and showing what the
 * coordinator says when it refuses. The form supplies its fields and payload.
 */
export function JobForm({
  targetsHeading = 'Targets',
  verb,
  targets,
  devices,
  groups,
  targetActions,
  build,
  submitLabel,
  confirm,
  acceptedByRestricted = false,
  onCreated,
  onCancel,
  children,
}: Props) {
  const [targetError, setTargetError] = useState<string | null>(null)
  const [apiError, setApiError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [pending, setPending] = useState<JobBody | null>(null)
  const ids = useId()

  const resolved = targets ? resolveTargets(targets, devices, groups) : null
  const count = resolved?.devices.length ?? 0
  const restricted = acceptedByRestricted ? 0 : (resolved?.devices.filter((d) => d.restricted).length ?? 0)

  async function send(body: JobBody) {
    if (!targets || !resolved || submitting) return
    setSubmitting(true)
    setApiError(null)
    try {
      const { job } = await api.createJob({ ...body, targets: requestTargets(targets, resolved) } as CreateJob)
      onCreated(job)
    } catch (err) {
      setApiError(errorMessage(err))
      setSubmitting(false)
    }
  }

  function submit() {
    if (submitting) return
    setApiError(null)
    const body = build()
    const noTargets = !targets || !resolved || count === 0
    setTargetError(noTargets ? noTargetsMessage(targets, verb) : null)
    if (!body || noTargets) return
    if (confirm) setPending(body)
    else void send(body)
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault()
    submit()
  }

  const confirmation = pending && confirm ? confirm(count) : null

  return (
    <>
      <form className="composer" onSubmit={onSubmit} noValidate>
        <section className="panel" aria-labelledby={`${ids}-targets`}>
          <h2 id={`${ids}-targets`}>{targetsHeading}</h2>
          {targets ? <TargetSummary spec={targets} devices={devices} groups={groups} /> : <p>No devices selected.</p>}
          {restricted > 0 ? (
            <p className="muted">
              {plural(restricted, 'device')} {restricted === 1 ? 'is' : 'are'} restricted (the client is too old for this
              coordinator) and only {restricted === 1 ? 'accepts' : 'accept'} client updates, so{' '}
              {restricted === 1 ? 'it' : 'they'} will not run this job.
            </p>
          ) : null}
          {targetActions ? <div className="target-actions">{targetActions}</div> : null}
          {targetError ? (
            <p className="error" role="alert">
              {targetError}
            </p>
          ) : null}
        </section>

        {children}

        {apiError && !confirmation ? (
          <p className="error" role="alert">
            {apiError}
          </p>
        ) : null}

        <div className="form-actions">
          <button type="submit" className="primary" disabled={submitting}>
            {submitting ? 'Starting…' : submitLabel(count)}
          </button>
          {onCancel ? (
            <button type="button" onClick={onCancel}>
              Cancel
            </button>
          ) : null}
        </div>
      </form>
      {confirmation ? (
        <ConfirmDialog
          title={confirmation.title}
          confirmLabel={confirmation.confirmLabel}
          danger={confirmation.danger}
          busy={submitting}
          error={apiError}
          onConfirm={() => pending && void send(pending)}
          onCancel={() => {
            setPending(null)
            setApiError(null)
          }}
        >
          {confirmation.body}
        </ConfirmDialog>
      ) : null}
    </>
  )
}
