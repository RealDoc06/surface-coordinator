import { useId, useRef, useState, type ReactNode } from 'react'
import type { Device, Group, Job, TargetSpec } from '../api/types'
import {
  MAX_ARGUMENT_CHARS,
  MAX_ARGUMENTS,
  MAX_PATH_CHARS,
  validateLaunch,
  type LaunchErrors,
} from '../lib/launch'
import { plural } from '../lib/format'
import type { LaunchPrefill } from '../lib/targets'
import { JobForm } from './JobForm'

type Props = {
  targets: TargetSpec | null
  devices: Device[]
  groups?: Group[]
  initial?: LaunchPrefill
  targetActions?: ReactNode
  onCreated: (job: Job) => void
  onCancel?: () => void
}

/** The form that creates an app.launch job. */
export function LaunchAppForm({ targets, devices, groups, initial, targetActions, onCreated, onCancel }: Props) {
  const [path, setPath] = useState(initial?.path ?? '')
  const [args, setArgs] = useState<string[]>(initial?.arguments ?? [])
  const [errors, setErrors] = useState<LaunchErrors>({})
  const ids = useId()
  const rows = useRef<(HTMLInputElement | null)[]>([])
  const focusRow = useRef<number | null>(null)

  function setArg(index: number, value: string) {
    setArgs((prev) => prev.map((a, i) => (i === index ? value : a)))
  }

  function addArg() {
    focusRow.current = args.length
    setArgs((prev) => [...prev, ''])
  }

  function removeArg(index: number) {
    setArgs((prev) => prev.filter((_, i) => i !== index))
    setErrors((e) => ({ ...e, arguments: undefined }))
  }

  return (
    <JobForm
      verb="launch the app on"
      targets={targets}
      devices={devices}
      groups={groups}
      targetActions={targetActions}
      onCreated={onCreated}
      onCancel={onCancel}
      submitLabel={(n) => (n > 0 ? `Launch on ${plural(n, 'device')}` : 'Launch')}
      build={() => {
        const { payload, errors: found } = validateLaunch({ path, arguments: args })
        setErrors(found)
        return payload ? { kind: 'app.launch', payload } : null
      }}
    >
      <section className="panel" aria-labelledby={`${ids}-app`}>
        <h2 id={`${ids}-app`}>Program</h2>
        <p className="muted">
          The program starts in the session of the user who is logged in on the device. If nobody is logged in the job
          fails with “no user session”.
        </p>

        <div className="field">
          <label htmlFor={`${ids}-path`}>Path</label>
          <input
            id={`${ids}-path`}
            type="text"
            className="input-wide mono"
            placeholder="C:\Program Files\App\app.exe"
            autoComplete="off"
            autoCapitalize="off"
            spellCheck={false}
            value={path}
            onChange={(e) => setPath(e.target.value)}
            aria-invalid={errors.path ? true : undefined}
            aria-describedby={errors.path ? `${ids}-path-error` : `${ids}-path-hint`}
          />
          {errors.path ? (
            <p id={`${ids}-path-error`} className="error" role="alert">
              {errors.path}
            </p>
          ) : (
            <p id={`${ids}-path-hint`} className="hint">
              Full path on the device, up to {MAX_PATH_CHARS} characters.
            </p>
          )}
        </div>

        <fieldset className="arg-editor">
          <legend>Arguments</legend>
          {args.length === 0 ? <p className="hint">No arguments.</p> : null}
          {args.map((value, index) => (
            <div key={index} className="arg-row">
              <input
                ref={(el) => {
                  rows.current[index] = el
                  if (el && focusRow.current === index) {
                    focusRow.current = null
                    el.focus()
                  }
                }}
                type="text"
                className="mono"
                aria-label={`Argument ${index + 1}`}
                autoComplete="off"
                autoCapitalize="off"
                spellCheck={false}
                value={value}
                onChange={(e) => setArg(index, e.target.value)}
                aria-invalid={errors.arguments?.[index] ? true : undefined}
              />
              <button type="button" aria-label={`Remove argument ${index + 1}`} onClick={() => removeArg(index)}>
                Remove
              </button>
              {errors.arguments?.[index] ? (
                <p className="error arg-error" role="alert">
                  {errors.arguments[index]}
                </p>
              ) : null}
            </div>
          ))}
          <div className="arg-footer">
            <button type="button" onClick={addArg} disabled={args.length >= MAX_ARGUMENTS}>
              Add argument
            </button>
            <span className="hint">
              Each argument is passed as it is, up to {MAX_ARGUMENT_CHARS} characters; at most {MAX_ARGUMENTS}. Blank
              rows are ignored.
            </span>
          </div>
          {errors.count ? (
            <p className="error" role="alert">
              {errors.count}
            </p>
          ) : null}
        </fieldset>
      </section>
    </JobForm>
  )
}
