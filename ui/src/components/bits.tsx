// Small presentational pieces shared by the pages.

import { useRef, useState } from 'react'
import type { Device, Group, Target, TargetState } from '../api/types'
import { isCancelled } from '../lib/reasons'

export function StatusDot({ online }: { online: boolean }) {
  return (
    <span className={`dot ${online ? 'dot-online' : 'dot-offline'}`} role="img" aria-label={online ? 'Online' : 'Offline'} />
  )
}

const STATE_LABELS: Record<TargetState | 'cancelled', string> = {
  queued: 'queued',
  sent: 'sent',
  running: 'running',
  succeeded: 'succeeded',
  failed: 'failed',
  cancelled: 'cancelled',
  expired: 'expired',
}

/**
 * A cancelled target is `failed` with reason `cancelled` on the wire; the
 * operator asked for it, so it is shown as "cancelled" in a neutral colour.
 */
function displayState(target: Pick<Target, 'state'> & { reason?: string | null }): string {
  return isCancelled({ state: target.state, reason: target.reason ?? null }) ? 'cancelled' : target.state
}

export function StateBadge({ state, reason }: { state: TargetState; reason?: string | null }) {
  const shown = displayState({ state, reason })
  // A state this UI does not know yet (newer coordinator) still gets shown.
  return <span className={`badge state-${shown}`}>{STATE_LABELS[shown as TargetState] ?? shown}</span>
}

const STATE_ORDER: string[] = ['succeeded', 'failed', 'cancelled', 'running', 'sent', 'queued', 'expired']

/** "2 succeeded · 1 running · 1 queued" */
export function StateCounts({ targets }: { targets: (Pick<Target, 'state'> & { reason?: string | null })[] }) {
  if (targets.length === 0) return <span className="muted">no devices</span>
  const counts = new Map<string, number>()
  for (const t of targets) {
    const shown = displayState(t)
    counts.set(shown, (counts.get(shown) ?? 0) + 1)
  }
  const order = [...STATE_ORDER, ...[...counts.keys()].filter((s) => !STATE_ORDER.includes(s))]
  return (
    <span className="state-counts">
      {order
        .filter((s) => counts.has(s))
        .map((s) => (
          <span key={s} className={`count state-${s}`}>
            {counts.get(s)} {s}
          </span>
        ))}
    </span>
  )
}

export function ErrorNote({ message, onRetry }: { message: string | null | undefined; onRetry?: () => void }) {
  if (!message) return null
  return (
    <p className="error" role="alert">
      {message}{' '}
      {onRetry ? (
        <button type="button" className="link" onClick={onRetry}>
          Try again
        </button>
      ) : null}
    </p>
  )
}

function CopyIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <rect x="5" y="5" width="9" height="9" rx="1.5" fill="none" stroke="currentColor" strokeWidth="1.5" />
      <path d="M11 5V3.5A1.5 1.5 0 0 0 9.5 2h-6A1.5 1.5 0 0 0 2 3.5v6A1.5 1.5 0 0 0 3.5 11H5" fill="none" stroke="currentColor" strokeWidth="1.5" />
    </svg>
  )
}

/** A read-only value with a Copy button. Works without the Clipboard API (plain http). */
export function CopyField({ label, value }: { label: string; value: string }) {
  const input = useRef<HTMLInputElement>(null)
  const [copied, setCopied] = useState(false)

  async function copy() {
    let ok = false
    try {
      if (navigator.clipboard) {
        await navigator.clipboard.writeText(value)
        ok = true
      }
    } catch {
      ok = false
    }
    if (!ok && input.current) {
      // The Clipboard API needs a secure context; the UI is often on plain http.
      input.current.focus()
      input.current.select()
      try {
        ok = document.execCommand('copy')
      } catch {
        ok = false
      }
    }
    if (ok) {
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    }
  }

  return (
    <label className="copy-field">
      <span className="copy-label">{label}</span>
      <span className="copy-row">
        <input ref={input} className="mono" type="text" readOnly value={value} onFocus={(e) => e.currentTarget.select()} />
        <button type="button" onClick={() => void copy()}>
          <CopyIcon /> {copied ? 'Copied' : 'Copy'}
        </button>
      </span>
    </label>
  )
}

/** The client version, with "update available" and "restricted" markers when the coordinator says so. */
export function ClientVersion({ device }: { device: Pick<Device, 'client_version' | 'update_available' | 'restricted'> }) {
  return (
    <span className="version-cell">
      {device.client_version}
      {device.update_available ? (
        <span className="badge badge-info" title="A newer client version has been downloaded and can be installed on this device.">
          update available
        </span>
      ) : null}
      {device.restricted ? (
        <span
          className="badge badge-warn"
          title="This device runs a client too old for this coordinator. It only accepts client updates until it is updated."
        >
          restricted
        </span>
      ) : null}
    </span>
  )
}

/** The groups a device belongs to, as small chips (by name; ids of unknown groups are skipped). */
export function GroupChips({ device, groups }: { device: Pick<Device, 'group_ids'>; groups: Group[] }) {
  const ids = new Set(device.group_ids ?? [])
  const named = groups.filter((g) => ids.has(g.id))
  if (named.length === 0) return <span className="muted">&mdash;</span>
  return (
    <span className="chips">
      {named.map((g) => (
        <span key={g.id} className="chip">
          {g.name}
        </span>
      ))}
    </span>
  )
}
