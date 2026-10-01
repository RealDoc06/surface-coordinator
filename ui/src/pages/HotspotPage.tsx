import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import type { Hotspot, Key } from '../api/types'
import { CopyField, ErrorNote } from '../components/bits'
import { shortId } from '../lib/format'
import { useSession } from '../session/SessionProvider'
import { useStore } from '../state/StoreProvider'

/** While the hotspot is starting there is no event for it, so ask again this often. */
export const STARTING_POLL_MS = 2000

/** The message for a failed start, which has its own codes (docs/api.md, Hotspot). */
export function startErrorMessage(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.code === 'not_found') return 'That enrollment key does not exist any more. Choose another one.'
    if (err.code === 'hotspot_failed') return `The hotspot could not be started: ${err.message}`
  }
  return errorMessage(err)
}

export function HotspotPage() {
  const session = useSession()
  return (
    <>
      <div className="page-head">
        <h1>Hotspot</h1>
      </div>
      {session.capabilities.hotspot ? (
        <HotspotPanel />
      ) : (
        <div className="empty">
          <h2>Hotspot control is not available here</h2>
          <p>
            The hotspot is controlled from the manager application, on the computer that runs the coordinator. This
            coordinator is not running inside it.
          </p>
        </div>
      )}
    </>
  )
}

function HotspotPanel() {
  const { state, reloadKeys } = useStore()
  const [hotspot, setHotspot] = useState<Hotspot | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [busy, setBusy] = useState<'start' | 'stop' | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [choice, setChoice] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setHotspot(await api.getHotspot())
      setLoadError(null)
    } catch (err) {
      setLoadError(errorMessage(err))
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load, state.syncEpoch])

  // Keys are listed by the store from the start; look again so one created or
  // revoked a moment ago is not offered (or missing).
  useEffect(() => {
    void reloadKeys().catch(() => {})
  }, [reloadKeys])

  const starting = hotspot?.state === 'starting'
  useEffect(() => {
    if (!starting) return
    const timer = setInterval(() => void load(), STARTING_POLL_MS)
    return () => clearInterval(timer)
  }, [starting, load])

  const activeKeys = useMemo(() => (state.keys ?? []).filter((k) => k.revoked_at === null), [state.keys])
  const labels = useMemo(() => new Map((state.keys ?? []).map((k) => [k.id, k.label])), [state.keys])
  const selectedKey =
    activeKeys.find((k) => k.id === choice)?.id ??
    activeKeys.find((k) => k.id === hotspot?.key_id)?.id ??
    activeKeys[0]?.id ??
    ''

  async function start() {
    if (busy || selectedKey === '') return
    setBusy('start')
    setActionError(null)
    try {
      setHotspot(await api.startHotspot(selectedKey))
    } catch (err) {
      setActionError(startErrorMessage(err))
      if (err instanceof ApiError && (err.code === 'not_found' || err.code === 'key_revoked')) {
        void reloadKeys().catch(() => {})
      }
    } finally {
      setBusy(null)
    }
    await load()
  }

  async function stop() {
    if (busy) return
    setBusy('stop')
    setActionError(null)
    try {
      setHotspot(await api.stopHotspot())
    } catch (err) {
      setActionError(errorMessage(err))
    } finally {
      setBusy(null)
    }
    await load()
  }

  if (hotspot === null) {
    return loadError ? <ErrorNote message={loadError} onRetry={() => void load()} /> : <p className="muted">Loading&hellip;</p>
  }

  const keyLabel = hotspot.key_id ? (labels.get(hotspot.key_id) ?? shortId(hotspot.key_id)) : null

  return (
    <>
      {hotspot.control === 'unsupported' ? null : <Explanation hotspot={hotspot} />}
      <ErrorNote message={loadError} onRetry={() => void load()} />

      {hotspot.control === 'unsupported' ? (
        <section className="panel" aria-label="Hotspot not supported">
          <h2>This computer cannot run a hotspot for you</h2>
          <p>
            The manager cannot create a Wi-Fi hotspot on this operating system or with this Wi-Fi hardware. Use
            another way to give the devices a network to meet on (a phone hotspot or a router, for example), and use{' '}
            <Link to="/">Move to Wi-Fi&hellip;</Link> on the Devices page to send devices to it.
          </p>
        </section>
      ) : hotspot.state === 'on' ? (
        <OnPanel
          hotspot={hotspot}
          keyLabel={keyLabel}
          stopping={busy === 'stop'}
          onStop={() => void stop()}
        />
      ) : hotspot.state === 'starting' || busy === 'start' ? (
        <section className="panel" aria-label="Hotspot starting" aria-busy="true">
          <p role="status">Starting the hotspot&hellip; this can take a few seconds.</p>
        </section>
      ) : (
        <StartPanel
          hotspot={hotspot}
          keys={activeKeys}
          selected={selectedKey}
          onSelect={setChoice}
          onStart={() => void start()}
        />
      )}

      {actionError ? (
        <p className="error" role="alert">
          {actionError}
        </p>
      ) : null}
    </>
  )
}

function Explanation({ hotspot }: { hotspot: Hotspot }) {
  return (
    <p className="lead">
      Devices enrolled with the chosen enrollment key look for the hotspot&rsquo;s network, join it and connect to this
      computer on port {hotspot.client_port}; they then appear under <Link to="/">Devices</Link>. Next, use{' '}
      <span className="nowrap">&ldquo;Move to Wi-Fi&hellip;&rdquo;</span> there to send them on to the network they should end up on.
      {hotspot.control === 'manual'
        ? ' On this computer you turn the hotspot on yourself; the manager only tells you what to set up.'
        : ''}
    </p>
  )
}

type StartPanelProps = {
  hotspot: Hotspot
  keys: Key[]
  selected: string
  onSelect: (id: string) => void
  onStart: () => void
}

function StartPanel({ hotspot, keys, selected, onSelect, onStart }: StartPanelProps) {
  const manual = hotspot.control === 'manual'
  const failed = hotspot.state === 'error'
  return (
    <section className="panel" aria-label="Start the hotspot">
      <h2>{failed ? 'The hotspot did not start' : manual ? 'Hotspot settings' : 'Hotspot is off'}</h2>
      {failed ? (
        <p className="error" role="alert">
          {hotspot.error ?? 'The hotspot could not be started.'}
        </p>
      ) : null}
      {keys.length === 0 ? (
        <p>
          There is no active enrollment key. A hotspot is started for one key, and only devices enrolled with it will
          find it. Create one under <Link to="/keys">Enrollment keys</Link> first.
        </p>
      ) : (
        <>
          <div className="field">
            <label htmlFor="hotspot-key">Enrollment key</label>
            <select id="hotspot-key" value={selected} onChange={(e) => onSelect(e.target.value)}>
              {keys.map((key) => (
                <option key={key.id} value={key.id}>
                  {key.label} ({shortId(key.id)})
                </option>
              ))}
            </select>
            <p className="hint">The hotspot&rsquo;s name and passphrase are derived from this key.</p>
          </div>
          <div className="form-actions">
            <button type="button" className="primary" onClick={onStart}>
              {failed ? 'Try again' : manual ? 'Show hotspot settings' : 'Start hotspot'}
            </button>
          </div>
        </>
      )}
    </section>
  )
}

type OnPanelProps = {
  hotspot: Hotspot
  keyLabel: string | null
  stopping: boolean
  onStop: () => void
}

function OnPanel({ hotspot, keyLabel, stopping, onStop }: OnPanelProps) {
  const manual = hotspot.control === 'manual'
  return (
    <section className="panel" aria-label="Hotspot settings">
      <h2>{manual ? 'Turn the hotspot on yourself' : 'Hotspot is on'}</h2>
      {manual ? (
        <>
          <p>
            The manager cannot switch the hotspot on for this computer: you do it, with these settings. The devices
            join it once it is running.
          </p>
          {hotspot.instructions.length > 0 ? (
            <ol className="steps" aria-label="Steps">
              {hotspot.instructions.map((step, i) => (
                <li key={i}>{step}</li>
              ))}
            </ol>
          ) : null}
        </>
      ) : null}
      <CopyField label="Network name (SSID)" value={hotspot.ssid ?? ''} />
      <CopyField label="Passphrase" value={hotspot.passphrase ?? ''} />
      <dl className="fields">
        <div>
          <dt>Devices connect on port</dt>
          <dd className="mono">{hotspot.client_port}</dd>
        </div>
        {keyLabel ? (
          <div>
            <dt>For enrollment key</dt>
            <dd>{keyLabel}</dd>
          </div>
        ) : null}
      </dl>
      {manual ? (
        <p className="hint">Stopping here does not turn the hotspot off: do that yourself as well.</p>
      ) : null}
      <div className="form-actions">
        <button type="button" className="danger" disabled={stopping} onClick={onStop}>
          {stopping ? 'Stopping…' : 'Stop hotspot'}
        </button>
      </div>
    </section>
  )
}
