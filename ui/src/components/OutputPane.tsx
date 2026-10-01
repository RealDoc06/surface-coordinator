import { useLayoutEffect, useRef } from 'react'

/** How close to the end still counts as "at the bottom", in pixels. */
const STICK_THRESHOLD = 8

type Props = {
  /** Changes when a different output is shown; the pane then starts at the end again. */
  streamKey: string
  text: string
  truncated: boolean
  /** Shown instead of the output while there is none. */
  placeholder: string
  label: string
}

/**
 * Monospace output that follows new text only while the reader is at the
 * bottom; scrolling up to read stops it, scrolling back down resumes it.
 */
export function OutputPane({ streamKey, text, truncated, placeholder, label }: Props) {
  const pane = useRef<HTMLPreElement>(null)
  const stick = useRef(true)
  const lastKey = useRef(streamKey)

  useLayoutEffect(() => {
    const el = pane.current
    if (!el) return
    if (lastKey.current !== streamKey) {
      lastKey.current = streamKey
      stick.current = true
    }
    if (stick.current) el.scrollTop = el.scrollHeight
  }, [streamKey, text])

  function onScroll() {
    const el = pane.current
    if (!el) return
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight <= STICK_THRESHOLD
  }

  return (
    <div className="output">
      <pre ref={pane} className="output-pane" tabIndex={0} aria-label={label} onScroll={onScroll}>
        {text === '' ? <span className="muted">{placeholder}</span> : text}
      </pre>
      {truncated ? (
        <p className="notice" role="note">
          Output was truncated: the device produced more than the coordinator stores per device.
        </p>
      ) : null}
    </div>
  )
}
