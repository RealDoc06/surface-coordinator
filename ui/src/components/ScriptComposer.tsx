import { useId, useState, type FormEvent, type KeyboardEvent, type ReactNode } from 'react'
import { api, errorMessage } from '../api/client'
import type { CreateJob, Device, Group, Job, RunAs, ScriptRun, Shell, TargetSpec } from '../api/types'
import { plural } from '../lib/format'
import { DEFAULT_TIMEOUT_SECS, RUN_AS_LABELS, SHELL_LABELS } from '../lib/jobKinds'
import { noTargetsMessage, requestTargets, resolveTargets } from '../lib/targets'
import { SaveToLibraryDialog, ScriptPicker } from './ScriptLibraryDialogs'
import { TargetSummary } from './TargetSummary'

export const MAX_TIMEOUT_SECS = 86_400

type Props = {
  /** Where the job goes; null when nothing has been chosen yet. */
  targets: TargetSpec | null
  /** Every device the coordinator knows. */
  devices: Device[]
  /** Every group, to describe a group target. */
  groups?: Group[]
  /** Values to start from ("Run again"). */
  initial?: Partial<ScriptRun>
  /** Ways to change the targets, supplied by the page. */
  targetActions?: ReactNode
  onCreated: (job: Job) => void
  onCancel?: () => void
}

type Errors = { body?: string; timeout?: string; targets?: string }

/** Parses the timeout field: whole seconds, 1..86400. */
export function parseTimeout(text: string): number | null {
  const trimmed = text.trim()
  if (!/^\d+$/.test(trimmed)) return null
  const value = Number(trimmed)
  return value >= 1 && value <= MAX_TIMEOUT_SECS ? value : null
}

/** The form that creates a script.run job. */
export function ScriptComposer({ targets, devices, groups, initial, targetActions, onCreated, onCancel }: Props) {
  const [shell, setShell] = useState<Shell>(initial?.shell ?? 'powershell')
  const [body, setBody] = useState(initial?.body ?? '')
  const [timeout, setTimeoutText] = useState(String(initial?.timeout_secs ?? DEFAULT_TIMEOUT_SECS))
  const [runAs, setRunAs] = useState<RunAs>(initial?.run_as ?? 'system')
  const [errors, setErrors] = useState<Errors>({})
  const [apiError, setApiError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [picking, setPicking] = useState(false)
  const [saving, setSaving] = useState(false)
  const [libraryNote, setLibraryNote] = useState<string | null>(null)
  const ids = useId()

  const resolved = targets ? resolveTargets(targets, devices, groups) : null
  const targetCount = resolved?.devices.length ?? 0

  async function submit() {
    if (submitting) return
    const next: Errors = {}
    if (body.trim() === '') next.body = 'Enter the script to run.'
    const timeoutSecs = parseTimeout(timeout)
    if (timeoutSecs === null) next.timeout = `Enter a whole number of seconds from 1 to ${MAX_TIMEOUT_SECS}.`
    if (!targets || !resolved || targetCount === 0) {
      next.targets = noTargetsMessage(targets, 'run on')
    }
    setErrors(next)
    setApiError(null)
    if (Object.keys(next).length > 0 || !targets || !resolved || timeoutSecs === null) return

    const request: CreateJob = {
      kind: 'script.run',
      payload: { shell, body, timeout_secs: timeoutSecs, run_as: runAs },
      targets: requestTargets(targets, resolved),
    }
    setSubmitting(true)
    try {
      const { job } = await api.createJob(request)
      onCreated(job)
    } catch (err) {
      setApiError(errorMessage(err))
      setSubmitting(false)
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault()
    void submit()
  }

  function onBodyKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
      event.preventDefault()
      void submit()
    }
  }

  return (
    <>
      <form className="composer" onSubmit={onSubmit} noValidate>
        <section className="panel" aria-labelledby={`${ids}-targets`}>
          <h2 id={`${ids}-targets`}>Targets</h2>
          {targets ? <TargetSummary spec={targets} devices={devices} groups={groups} /> : <p>No devices selected.</p>}
          {targetActions ? <div className="target-actions">{targetActions}</div> : null}
          {errors.targets ? (
            <p className="error" role="alert">
              {errors.targets}
            </p>
          ) : null}
        </section>

        <section className="panel" aria-labelledby={`${ids}-script`}>
          <div className="section-head">
            <h2 id={`${ids}-script`}>Script</h2>
            <span className="toolbar-spacer" />
            <button type="button" onClick={() => setPicking(true)}>
              Load from library&hellip;
            </button>
            <button type="button" onClick={() => setSaving(true)}>
              Save to library&hellip;
            </button>
          </div>
          {libraryNote ? (
            <p className="success" role="status">
              {libraryNote}
            </p>
          ) : null}
          <div className="form-row">
            <fieldset className="choice">
              <legend>Shell</legend>
              {(Object.keys(SHELL_LABELS) as Shell[]).map((value) => (
                <label key={value}>
                  <input type="radio" name={`${ids}-shell`} checked={shell === value} onChange={() => setShell(value)} />
                  {SHELL_LABELS[value]}
                </label>
              ))}
            </fieldset>
            <fieldset className="choice">
              <legend>Run as</legend>
              {(Object.keys(RUN_AS_LABELS) as RunAs[]).map((value) => (
                <label key={value}>
                  <input type="radio" name={`${ids}-runas`} checked={runAs === value} onChange={() => setRunAs(value)} />
                  {RUN_AS_LABELS[value]}
                </label>
              ))}
            </fieldset>
            <div className="field">
              <label htmlFor={`${ids}-timeout`}>Timeout (seconds)</label>
              <input
                id={`${ids}-timeout`}
                type="text"
                inputMode="numeric"
                className="input-narrow"
                value={timeout}
                onChange={(e) => setTimeoutText(e.target.value)}
                aria-invalid={errors.timeout ? true : undefined}
                aria-describedby={errors.timeout ? `${ids}-timeout-error` : undefined}
              />
              {errors.timeout ? (
                <p id={`${ids}-timeout-error`} className="error" role="alert">
                  {errors.timeout}
                </p>
              ) : null}
            </div>
          </div>

          <div className="field">
            <label htmlFor={`${ids}-body`}>Script body</label>
            <textarea
              id={`${ids}-body`}
              className="mono script-body"
              rows={14}
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              value={body}
              onChange={(e) => setBody(e.target.value)}
              onKeyDown={onBodyKeyDown}
              aria-invalid={errors.body ? true : undefined}
              aria-describedby={errors.body ? `${ids}-body-error` : `${ids}-body-hint`}
            />
            {errors.body ? (
              <p id={`${ids}-body-error`} className="error" role="alert">
                {errors.body}
              </p>
            ) : (
              <p id={`${ids}-body-hint`} className="hint">
                Output and the exit code are reported per device. Ctrl+Enter runs the script.
              </p>
            )}
          </div>
        </section>

        {apiError ? (
          <p className="error" role="alert">
            {apiError}
          </p>
        ) : null}

        <div className="form-actions">
          <button type="submit" className="primary" disabled={submitting}>
            {submitting ? 'Starting…' : targetCount > 0 ? `Run on ${plural(targetCount, 'device')}` : 'Run'}
          </button>
          {onCancel ? (
            <button type="button" onClick={onCancel}>
              Cancel
            </button>
          ) : null}
        </div>
      </form>
      {picking ? (
        <ScriptPicker
          onClose={() => setPicking(false)}
          onPick={(script) => {
            setShell(script.shell)
            setBody(script.body)
            setErrors((e) => ({ ...e, body: undefined }))
            setLibraryNote(`Loaded “${script.name}” from the library.`)
            setPicking(false)
          }}
        />
      ) : null}
      {saving ? (
        <SaveToLibraryDialog
          shell={shell}
          body={body}
          onClose={() => setSaving(false)}
          onSaved={(script) => {
            setLibraryNote(`Saved to the library as “${script.name}”.`)
            setSaving(false)
          }}
        />
      ) : null}
    </>
  )
}
