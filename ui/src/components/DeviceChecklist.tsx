import { useEffect, useRef, useState } from 'react'
import type { Device } from '../api/types'
import { plural, shortId } from '../lib/format'
import { StatusDot } from './bits'

type Props = {
  /** Already sorted for display. */
  devices: Device[]
  selected: ReadonlySet<string>
  onChange: (selected: Set<string>) => void
  /** Accessible name of the list. */
  label: string
}

/** Devices with checkboxes and a filter, for choosing a set of them. */
export function DeviceChecklist({ devices, selected, onChange, label }: Props) {
  const [text, setText] = useState('')
  const needle = text.trim().toLowerCase()
  const shown = devices.filter(
    (d) => needle === '' || d.hostname.toLowerCase().includes(needle) || d.id.toLowerCase().includes(needle),
  )
  const shownSelected = shown.filter((d) => selected.has(d.id)).length
  const allShown = shown.length > 0 && shownSelected === shown.length

  const master = useRef<HTMLInputElement>(null)
  useEffect(() => {
    if (master.current) master.current.indeterminate = shownSelected > 0 && !allShown
  }, [shownSelected, allShown])

  function toggle(id: string, on: boolean) {
    const next = new Set(selected)
    if (on) next.add(id)
    else next.delete(id)
    onChange(next)
  }

  function toggleShown(on: boolean) {
    const next = new Set(selected)
    for (const d of shown) {
      if (on) next.add(d.id)
      else next.delete(d.id)
    }
    onChange(next)
  }

  const known = new Set(devices.map((d) => d.id))
  const count = [...selected].filter((id) => known.has(id)).length

  return (
    <div className="checklist">
      <div className="toolbar">
        <label className="field-inline">
          <span className="visually-hidden">Filter devices</span>
          <input
            type="search"
            placeholder="Filter by hostname or id"
            value={text}
            onChange={(e) => setText(e.target.value)}
          />
        </label>
        <label className="field-inline">
          <input
            ref={master}
            type="checkbox"
            checked={allShown}
            disabled={shown.length === 0}
            onChange={(e) => toggleShown(e.target.checked)}
          />
          <span>Select all shown</span>
        </label>
        <span className="toolbar-spacer" />
        <strong aria-live="polite">{count} selected</strong>
      </div>
      {devices.length === 0 ? (
        <p className="muted">No devices have enrolled yet.</p>
      ) : (
        <ul className="checklist-items" aria-label={label}>
          {shown.map((d) => (
            <li key={d.id}>
              <label>
                <input
                  type="checkbox"
                  aria-label={d.hostname || shortId(d.id)}
                  checked={selected.has(d.id)}
                  onChange={(e) => toggle(d.id, e.target.checked)}
                />
                <StatusDot online={d.online} />
                <span>{d.hostname || shortId(d.id)}</span>
              </label>
            </li>
          ))}
          {shown.length === 0 ? (
            <li className="muted">No devices match the filter ({plural(devices.length, 'device')} in total).</li>
          ) : null}
        </ul>
      )}
    </div>
  )
}
