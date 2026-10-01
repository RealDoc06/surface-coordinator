import { useId, useState, type ReactNode } from 'react'
import type { Device, Group, Job, Setting, TargetSpec } from '../api/types'
import { codePointLength } from '../state/store'
import { plural } from '../lib/format'
import { initialValueText, isPercent, parseSettingValue, valueToText } from '../lib/settings'
import type { SettingPrefill } from '../lib/targets'
import { JobForm } from './JobForm'

type Props = {
  /** The settings catalogue; null when it could not be loaded. */
  catalogue: Setting[] | null
  targets: TargetSpec | null
  devices: Device[]
  groups?: Group[]
  initial?: SettingPrefill
  targetActions?: ReactNode
  onCreated: (job: Job) => void
  onCancel?: () => void
  onRetryCatalogue?: () => void
}

type Errors = { setting?: string; value?: string }

function startingText(setting: Setting | undefined, initial: SettingPrefill | undefined): string {
  if (!setting) return ''
  if (initial?.value !== undefined) {
    const text = valueToText(setting, initial.value)
    if (text !== null) return text
  }
  return initialValueText(setting)
}

/** The form that creates a setting.apply job. */
export function SettingForm({
  catalogue,
  targets,
  devices,
  groups,
  initial,
  targetActions,
  onCreated,
  onCancel,
  onRetryCatalogue,
}: Props) {
  const [settingId, setSettingId] = useState(initial?.setting ?? '')
  const setting = catalogue?.find((s) => s.id === settingId)
  const [valueText, setValueText] = useState(() => startingText(setting, initial))
  const [errors, setErrors] = useState<Errors>({})
  const ids = useId()

  function choose(id: string) {
    setSettingId(id)
    setValueText(startingText(catalogue?.find((s) => s.id === id), undefined))
    setErrors({})
  }

  if (catalogue === null || catalogue.length === 0) {
    return (
      <section className="panel">
        <h2>Setting</h2>
        <p className="muted">
          {catalogue === null
            ? 'The list of settings could not be loaded. The coordinator may be an older version, or still starting.'
            : 'This coordinator offers no settings to apply.'}{' '}
          {onRetryCatalogue ? (
            <button type="button" className="link" onClick={onRetryCatalogue}>
              Try again
            </button>
          ) : null}
        </p>
      </section>
    )
  }

  return (
    <JobForm
      verb="apply the setting to"
      targets={targets}
      devices={devices}
      groups={groups}
      targetActions={targetActions}
      onCreated={onCreated}
      onCancel={onCancel}
      submitLabel={(n) => (n > 0 ? `Apply to ${plural(n, 'device')}` : 'Apply')}
      build={() => {
        const next: Errors = {}
        if (!setting) {
          next.setting = 'Choose a setting.'
          setErrors(next)
          return null
        }
        const parsed = parseSettingValue(setting, valueText)
        if (parsed.error !== undefined) next.value = parsed.error
        setErrors(next)
        return parsed.error === undefined
          ? { kind: 'setting.apply', payload: { setting: setting.id, value: parsed.value } }
          : null
      }}
    >
      <section className="panel" aria-labelledby={`${ids}-setting`}>
        <h2 id={`${ids}-setting`}>Apply a setting</h2>

        <div className="field">
          <label htmlFor={`${ids}-pick`}>Setting</label>
          <select
            id={`${ids}-pick`}
            className="input-wide"
            value={settingId}
            onChange={(e) => choose(e.target.value)}
            aria-invalid={errors.setting ? true : undefined}
          >
            <option value="">Choose a setting…</option>
            {catalogue.map((s) => (
              <option key={s.id} value={s.id}>
                {s.label}
              </option>
            ))}
          </select>
          {errors.setting ? (
            <p className="error" role="alert">
              {errors.setting}
            </p>
          ) : null}
        </div>

        {settingId !== '' && !setting ? (
          <p className="error" role="alert">
            The coordinator does not offer a setting “{settingId}”. Choose another one.
          </p>
        ) : null}

        {setting ? (
          <>
            <p>{setting.description}</p>
            {setting.scope === 'user' ? (
              <p className="muted">
                This setting belongs to a user: someone has to be logged in on the device, otherwise the job fails with
                “no user session”.
              </p>
            ) : (
              <p className="muted">This setting applies to the whole machine; nobody needs to be logged in.</p>
            )}
            <ValueEditor
              idBase={ids}
              setting={setting}
              text={valueText}
              error={errors.value}
              onChange={(text) => {
                setValueText(text)
                setErrors((e) => ({ ...e, value: undefined }))
              }}
            />
          </>
        ) : null}
      </section>
    </JobForm>
  )
}

function ValueEditor({
  idBase,
  setting,
  text,
  error,
  onChange,
}: {
  idBase: string
  setting: Setting
  text: string
  error?: string
  onChange: (text: string) => void
}) {
  const id = `${idBase}-value`
  const describedBy = error ? `${id}-error` : undefined
  const value = setting.value
  const errorNote = error ? (
    <p id={`${id}-error`} className="error" role="alert">
      {error}
    </p>
  ) : null

  switch (value.type) {
    case 'bool':
      return (
        <div className="field">
          <span className="field-label" id={`${id}-label`}>
            Value
          </span>
          <label className="switch">
            <input
              type="checkbox"
              role="switch"
              aria-labelledby={`${id}-label`}
              checked={text === 'on'}
              onChange={(e) => onChange(e.target.checked ? 'on' : 'off')}
            />
            <span className="switch-track" aria-hidden="true" />
            <span>{text === 'on' ? 'On' : 'Off'}</span>
          </label>
        </div>
      )

    case 'integer': {
      const label = value.unit ? `Value (${value.unit})` : 'Value'
      const n = Number(text)
      const sliderValue = Number.isFinite(n) ? Math.min(value.max, Math.max(value.min, n)) : value.min
      return (
        <div className="field">
          <label htmlFor={id}>{label}</label>
          <div className="number-row">
            {isPercent(setting) ? (
              <input
                type="range"
                aria-label={`${setting.label} slider`}
                min={value.min}
                max={value.max}
                step={1}
                value={sliderValue}
                onChange={(e) => onChange(e.target.value)}
              />
            ) : null}
            <input
              id={id}
              type="number"
              inputMode="numeric"
              className="input-narrow"
              min={value.min}
              max={value.max}
              step={1}
              value={text}
              onChange={(e) => onChange(e.target.value)}
              aria-invalid={error ? true : undefined}
              aria-describedby={describedBy}
            />
            {value.unit ? <span className="muted">{value.unit}</span> : null}
          </div>
          <p className="hint">
            {value.min} to {value.max}
            {value.unit ? ` ${value.unit}` : ''}.
          </p>
          {errorNote}
        </div>
      )
    }

    case 'text':
      return (
        <div className="field">
          <label htmlFor={id}>Value</label>
          <input
            id={id}
            type="text"
            className="input-wide"
            autoComplete="off"
            spellCheck={false}
            value={text}
            onChange={(e) => onChange(e.target.value)}
            aria-invalid={error ? true : undefined}
            aria-describedby={describedBy}
          />
          <p className="hint">
            Up to {value.max_len} characters ({codePointLength(text)} used).
          </p>
          {errorNote}
        </div>
      )
  }
}
