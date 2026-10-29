import { useId, useState, type FormEvent, type ReactNode } from 'react'
import { api, errorMessage } from '../api/client'
import type { CreateJob, Device, Group, Job, TargetSpec } from '../api/types'
import { plural } from '../lib/format'
import { noTargetsMessage, requestTargets, resolveTargets, type WifiPrefill } from '../lib/targets'
import {
  PASSWORD_MAX_CHARS,
  PASSWORD_MIN_CHARS,
  SSID_MAX_BYTES,
  utf8Length,
  validateWifiForm,
  wifiPayload,
  type WifiErrors,
} from '../lib/wifi'
import { TargetSummary } from './TargetSummary'

type Props = {
  /** Where the job goes; null when nothing has been chosen yet. */
  targets: TargetSpec | null
  /** Every device the coordinator knows. */
  devices: Device[]
  /** Every group, to describe a group target. */
  groups?: Group[]
  /** Values to start from ("Run again"). */
  initial?: WifiPrefill
  /** Ways to change the targets, supplied by the page. */
  targetActions?: ReactNode
  onCreated: (job: Job) => void
  onCancel?: () => void
}

type Errors = WifiErrors & { targets?: string }

/** The form that creates a wifi.join job: move devices to another Wi-Fi network. */
export function WifiJoinForm({ targets, devices, groups, initial, targetActions, onCreated, onCancel }: Props) {
  const [ssid, setSsid] = useState(initial?.ssid ?? '')
  const [password, setPassword] = useState('')
  const [showPassword, setShowPassword] = useState(false)
  const [coordinatorUrl, setCoordinatorUrl] = useState(initial?.coordinator_url ?? '')
  const [errors, setErrors] = useState<Errors>({})
  const [apiError, setApiError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const ids = useId()

  const resolved = targets ? resolveTargets(targets, devices, groups) : null
  const targetCount = resolved?.devices.length ?? 0
  const ssidBytes = utf8Length(ssid)

  async function submit() {
    if (submitting) return
    const form = { ssid, password, coordinatorUrl }
    const next: Errors = validateWifiForm(form)
    if (!targets || !resolved || targetCount === 0) {
      next.targets = noTargetsMessage(targets, 'move')
    }
    setErrors(next)
    setApiError(null)
    if (Object.keys(next).length > 0 || !targets || !resolved) return

    const request: CreateJob = {
      kind: 'wifi.join',
      payload: wifiPayload(form),
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

  const edgeSpace = ssid !== '' && ssid.trim() !== ssid

  return (
    <form className="composer" onSubmit={onSubmit} noValidate>
      <div className="notice warning" role="note">
        <p>
          <strong>These devices will leave the network they are on now.</strong> Make sure the coordinator address
          is reachable from the destination network. Devices already connected manually do not need a Wi-Fi job.
        </p>
        <p>
          If a device moves to another coordinator and cannot report back here, the job shows “Destination
          unconfirmed” after two minutes. Check the destination coordinator or device before retrying.
        </p>
      </div>

      <section className="panel" aria-labelledby={`${ids}-targets`}>
        <h2 id={`${ids}-targets`}>Devices to move</h2>
        {targets ? <TargetSummary spec={targets} devices={devices} groups={groups} /> : <p>No devices selected.</p>}
        {targetActions ? <div className="target-actions">{targetActions}</div> : null}
        {errors.targets ? (
          <p className="error" role="alert">
            {errors.targets}
          </p>
        ) : null}
      </section>

      <section className="panel" aria-labelledby={`${ids}-network`}>
        <h2 id={`${ids}-network`}>Network</h2>

        <div className="field">
          <label htmlFor={`${ids}-ssid`}>Network name (SSID)</label>
          <input
            id={`${ids}-ssid`}
            type="text"
            className="input-wide"
            autoComplete="off"
            autoCapitalize="off"
            spellCheck={false}
            value={ssid}
            onChange={(e) => setSsid(e.target.value)}
            aria-invalid={errors.ssid ? true : undefined}
            aria-describedby={`${ids}-ssid-hint`}
          />
          {errors.ssid ? (
            <p className="error" role="alert">
              {errors.ssid}
            </p>
          ) : null}
          <p id={`${ids}-ssid-hint`} className="hint">
            Up to {SSID_MAX_BYTES} bytes ({ssidBytes} used).
            {edgeSpace ? ' The name starts or ends with a space; it is sent exactly as typed.' : ''}
          </p>
        </div>

        <div className="field">
          <label htmlFor={`${ids}-password`}>Password</label>
          <span className="copy-row">
            <input
              id={`${ids}-password`}
              type={showPassword ? 'text' : 'password'}
              className="input-wide mono"
              autoComplete="off"
              spellCheck={false}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              aria-invalid={errors.password ? true : undefined}
              aria-describedby={`${ids}-password-hint`}
            />
            <button type="button" aria-pressed={showPassword} onClick={() => setShowPassword((on) => !on)}>
              {showPassword ? 'Hide' : 'Show'}
            </button>
          </span>
          {errors.password ? (
            <p className="error" role="alert">
              {errors.password}
            </p>
          ) : null}
          <p id={`${ids}-password-hint`} className="hint">
            {PASSWORD_MIN_CHARS} to {PASSWORD_MAX_CHARS} characters. Leave empty for an open network (no password).
            {initial?.passwordNeeded
              ? ' The job you are repeating had a password; the coordinator does not give it back, so enter it again.'
              : ''}
          </p>
        </div>

        <div className="field">
          <label htmlFor={`${ids}-url`}>Coordinator address (optional)</label>
          <input
            id={`${ids}-url`}
            type="text"
            className="input-wide mono"
            placeholder="ws://192.168.137.1:8081"
            autoComplete="off"
            autoCapitalize="off"
            spellCheck={false}
            value={coordinatorUrl}
            onChange={(e) => setCoordinatorUrl(e.target.value)}
            aria-invalid={errors.coordinatorUrl ? true : undefined}
            aria-describedby={`${ids}-url-hint`}
          />
          {errors.coordinatorUrl ? (
            <p className="error" role="alert">
              {errors.coordinatorUrl}
            </p>
          ) : null}
          <p id={`${ids}-url-hint`} className="hint">
            Starts with ws:// or wss://. Devices will use this coordinator from then on, so it has to be reachable from
            the new network. Leave empty to keep the one they use now.
          </p>
        </div>
      </section>

      {apiError ? (
        <p className="error" role="alert">
          {apiError}
        </p>
      ) : null}

      <div className="form-actions">
        <button type="submit" className="primary" disabled={submitting}>
          {submitting
            ? 'Starting…'
            : targetCount > 0
              ? `Move ${plural(targetCount, 'device')} to this network`
              : 'Move to this network'}
        </button>
        {onCancel ? (
          <button type="button" onClick={onCancel}>
            Cancel
          </button>
        ) : null}
      </div>
    </form>
  )
}
