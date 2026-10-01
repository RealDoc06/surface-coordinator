// Small presentational pieces shared by the pages.

import { useRef, useState } from 'react'
import type { Target, TargetState } from '../api/types'

export function StatusDot({ online }: { online: boolean }) {
  return (
    <span className={`dot ${online ? 'dot-online' : 'dot-offline'}`} role="img" aria-label={online ? 'Online' : 'Offline'} />
  )
}

const STATE_LABELS: Record<TargetState, string> = {
  queued: 'queued',
  sent: 'sent',
  running: 'running',
  succeeded: 'succeeded',
  failed: 'failed',
  expired: 'expired',
}

export function StateBadge({ state }: { state: TargetState }) {
  // A state this UI does not know yet (newer coordinator) still gets shown.
  return <span className={`badge state-${state}`}>{STATE_LABELS[state] ?? state}</span>
}

const STATE_ORDER: TargetState[] = ['succeeded', 'failed', 'running', 'sent', 'queued', 'expired']

/** "2 succeeded · 1 running · 1 queued" */
export function StateCounts({ targets }: { targets: Pick<Target, 'state'>[] }) {
  if (targets.length === 0) return <span className="muted">no devices</span>
  const counts = new Map<string, number>()
  for (const t of targets) counts.set(t.state, (counts.get(t.state) ?? 0) + 1)
  const order = [...STATE_ORDER, ...[...counts.keys()].filter((s) => !STATE_ORDER.includes(s as TargetState))]
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
