import { useEffect, useMemo, useRef, useState } from 'react'
import { Link } from 'react-router-dom'
import type { Device, TargetSpec } from '../api/types'
import { formatDateTime, plural, relativeTime, shortId } from '../lib/format'
import { describeNetwork } from '../lib/wifi'
import { StatusDot } from './bits'

type StatusFilter = 'all' | 'online' | 'offline'

type Props = {
  /** Already sorted for display. */
  devices: Device[]
  /** Enrollment key labels by key id, where known. */
  keyLabels: Record<string, string>
  now: number
  initialSelected?: string[]
  /** The user asked to run something on these targets. */
  onRun: (targets: TargetSpec) => void
  /** The user asked to move these targets to another Wi-Fi network. */
  onMove: (targets: TargetSpec) => void
}

/** The network a device reported: SSID with its address beside it, or a dash. */
export function NetworkCell({ network }: { network: Device['network'] }) {
  const { primary, secondary, title } = describeNetwork(network)
  if (primary === null) return <span className="muted">&mdash;</span>
  return (
    <span title={title}>
      {primary}
      {secondary ? <span className="muted mono network-ip"> {secondary}</span> : null}
    </span>
  )
}

export function matchesFilter(device: Device, text: string, status: StatusFilter): boolean {
  if (status === 'online' && !device.online) return false
  if (status === 'offline' && device.online) return false
  const needle = text.trim().toLowerCase()
  if (needle === '') return true
  return device.hostname.toLowerCase().includes(needle) || device.id.toLowerCase().includes(needle)
}

/** The device table with its filters and the selection that jobs are run on. */
export function DeviceList({ devices, keyLabels, now, initialSelected, onRun, onMove }: Props) {
  const [text, setText] = useState('')
  const [status, setStatus] = useState<StatusFilter>('all')
  const [selected, setSelected] = useState<ReadonlySet<string>>(() => new Set(initialSelected))

  const shown = useMemo(() => devices.filter((d) => matchesFilter(d, text, status)), [devices, text, status])

  // Devices can disappear while selected (forgotten elsewhere): only count real ones.
  const selectedIds = useMemo(() => devices.filter((d) => selected.has(d.id)).map((d) => d.id), [devices, selected])
  const shownSelected = shown.filter((d) => selected.has(d.id)).length
  const hiddenSelected = selectedIds.length - shownSelected
  const allShownSelected = shown.length > 0 && shownSelected === shown.length

  const selectAll = useRef<HTMLInputElement>(null)
  useEffect(() => {
    if (selectAll.current) selectAll.current.indeterminate = shownSelected > 0 && !allShownSelected
  }, [shownSelected, allShownSelected])

  function toggle(id: string, on: boolean) {
    setSelected((prev) => {
      const next = new Set(prev)
      if (on) next.add(id)
      else next.delete(id)
      return next
    })
  }

  function toggleShown(on: boolean) {
    setSelected((prev) => {
      const next = new Set(prev)
      for (const d of shown) {
        if (on) next.add(d.id)
        else next.delete(d.id)
      }
      return next
    })
  }

  if (devices.length === 0) {
    return (
      <div className="empty">
        <h2>No devices yet</h2>
        <p>Devices appear here the first time their client connects. To enroll one:</p>
        <ol>
          <li>
            Create an enrollment key under <Link to="/keys">Enrollment keys</Link> and note its id and secret.
          </li>
          <li>
            Start the client on the device with that key id and secret and the address clients reach this coordinator on
            (<code>wss://</code> behind TLS), for example:
            <pre className="code">
              {`$env:SC_KEY_SECRET = '<key secret>'\nclient.exe --coordinator-url ws://<coordinator address> --key-id <key id>`}
            </pre>
          </li>
        </ol>
        <p>
          A client that is turned away shows up under <Link to="/rejected">Rejected connections</Link> with the reason.
        </p>
      </div>
    )
  }

  return (
    <div>
      <div className="toolbar">
        <label className="field-inline">
          <span className="visually-hidden">Filter devices by hostname or id</span>
          <input
            type="search"
            placeholder="Filter by hostname or id"
            value={text}
            onChange={(e) => setText(e.target.value)}
          />
        </label>
        <label className="field-inline">
          <span>Show</span>
          <select value={status} onChange={(e) => setStatus(e.target.value as StatusFilter)}>
            <option value="all">All</option>
            <option value="online">Online</option>
            <option value="offline">Offline</option>
          </select>
        </label>
        <span className="toolbar-spacer" />
        <span className="selection-count" aria-live="polite">
          <strong>{selectedIds.length} selected</strong>
          {hiddenSelected > 0 ? <span className="muted"> ({hiddenSelected} not shown)</span> : null}
        </span>
        {selectedIds.length > 0 ? (
          <button type="button" className="link" onClick={() => setSelected(new Set())}>
            Clear
          </button>
        ) : null}
        <button
          type="button"
          className="primary"
          disabled={selectedIds.length === 0}
          onClick={() => onRun({ type: 'devices', device_ids: selectedIds })}
        >
          Run script&hellip;
        </button>
        <button
          type="button"
          disabled={selectedIds.length === 0}
          onClick={() => onMove({ type: 'devices', device_ids: selectedIds })}
        >
          Move to Wi-Fi&hellip;
        </button>
        <button type="button" onClick={() => onRun({ type: 'all' })}>
          Run on all devices
        </button>
        <button type="button" onClick={() => onMove({ type: 'all' })}>
          Move all to Wi-Fi&hellip;
        </button>
      </div>

      <table className="table">
        <thead>
          <tr>
            <th className="col-check">
              <input
                ref={selectAll}
                type="checkbox"
                aria-label="Select all shown devices"
                checked={allShownSelected}
                disabled={shown.length === 0}
                onChange={(e) => toggleShown(e.target.checked)}
              />
            </th>
            <th className="col-status">Status</th>
            <th>Hostname</th>
            <th>OS</th>
            <th>Network</th>
            <th>Client</th>
            <th>Last seen</th>
            <th>Enrollment key</th>
          </tr>
        </thead>
        <tbody>
          {shown.map((device) => (
            <tr key={device.id} className={selected.has(device.id) ? 'selected' : undefined}>
              <td className="col-check">
                <input
                  type="checkbox"
                  aria-label={`Select ${device.hostname}`}
                  checked={selected.has(device.id)}
                  onChange={(e) => toggle(device.id, e.target.checked)}
                />
              </td>
              <td className="col-status">
                <StatusDot online={device.online} />
              </td>
              <td>
                <Link to={`/devices/${encodeURIComponent(device.id)}`}>{device.hostname || shortId(device.id)}</Link>
              </td>
              <td>{device.os}</td>
              <td>
                <NetworkCell network={device.network} />
              </td>
              <td>{device.client_version}</td>
              <td title={formatDateTime(device.last_seen_at)}>
                {device.online ? 'now' : relativeTime(device.last_seen_at, now)}
              </td>
              <td>
                {keyLabels[device.enrollment_key_id] ?? (
                  <span className="muted mono" title={device.enrollment_key_id}>
                    {shortId(device.enrollment_key_id)}
                  </span>
                )}
              </td>
            </tr>
          ))}
          {shown.length === 0 ? (
            <tr>
              <td colSpan={8} className="table-empty">
                No devices match the filter ({plural(devices.length, 'device')} in total).
              </td>
            </tr>
          ) : null}
        </tbody>
      </table>
    </div>
  )
}
