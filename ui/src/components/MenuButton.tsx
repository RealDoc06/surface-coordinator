import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from 'react'
import { NavLink } from 'react-router-dom'

export type MenuItem =
  | { separator: true; key: string }
  | {
      key: string
      label: string
      /** Runs when chosen (a button item). */
      onSelect?: () => void
      /** Navigates when chosen (a link item). */
      to?: string
      disabled?: boolean
      danger?: boolean
      /** Shown after the label, hidden from assistive technology. */
      badge?: ReactNode
    }

type Props = {
  label: string
  items: MenuItem[]
  className?: string
  /** Extra content inside the button after the label (a badge). */
  adornment?: ReactNode
  align?: 'left' | 'right'
  disabled?: boolean
}

const ITEM = '[role="menuitem"]:not(:disabled):not([aria-disabled="true"])'

/**
 * A button that opens a menu of actions or links. Keyboard: Enter, Space or
 * the arrow keys open it, the arrows/Home/End move, Escape closes and returns
 * to the button, Tab leaves. Clicking elsewhere closes it.
 */
export function MenuButton({ label, items, className, adornment, align = 'left', disabled = false }: Props) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const button = useRef<HTMLButtonElement>(null)
  const menuId = useId()

  useEffect(() => {
    if (!open) return
    root.current?.querySelector<HTMLElement>(ITEM)?.focus()
    const onDown = (event: MouseEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    return () => document.removeEventListener('mousedown', onDown)
  }, [open])

  function onButtonKeyDown(event: KeyboardEvent<HTMLButtonElement>) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      setOpen(true)
    }
  }

  // On the whole control, so Escape also works when no item can take focus.
  function onRootKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key === 'Escape' && open) {
      event.preventDefault()
      event.stopPropagation()
      setOpen(false)
      button.current?.focus()
    }
  }

  function onMenuKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const els = Array.from(root.current?.querySelectorAll<HTMLElement>(ITEM) ?? [])
    const index = els.indexOf(document.activeElement as HTMLElement)
    const focus = (i: number) => els[(i + els.length) % els.length]?.focus()
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault()
        focus(index + 1)
        break
      case 'ArrowUp':
        event.preventDefault()
        focus(index - 1)
        break
      case 'Home':
        event.preventDefault()
        focus(0)
        break
      case 'End':
        event.preventDefault()
        focus(els.length - 1)
        break
      case 'Tab':
        setOpen(false)
        break
    }
  }

  return (
    <div className="menu-root" ref={root} onKeyDown={onRootKeyDown}>
      <button
        ref={button}
        type="button"
        className={className}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        disabled={disabled}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={onButtonKeyDown}
      >
        {label}
        {adornment}
        <span aria-hidden="true" className="menu-caret">
          ▾
        </span>
      </button>
      {open ? (
        <div id={menuId} role="menu" aria-label={label} className={`menu menu-${align}`} onKeyDown={onMenuKeyDown}>
          {items.map((item) => {
            if ('separator' in item) return <hr key={item.key} role="separator" className="menu-separator" />
            const cls = `menu-item${item.danger ? ' danger' : ''}`
            const content = (
              <>
                {item.label}
                {item.badge ? <span aria-hidden="true">{item.badge}</span> : null}
              </>
            )
            if (item.to !== undefined) {
              return (
                <NavLink
                  key={item.key}
                  to={item.to}
                  role="menuitem"
                  tabIndex={-1}
                  className={cls}
                  onClick={() => setOpen(false)}
                >
                  {content}
                </NavLink>
              )
            }
            return (
              <button
                key={item.key}
                type="button"
                role="menuitem"
                tabIndex={-1}
                className={cls}
                disabled={item.disabled}
                onClick={() => {
                  setOpen(false)
                  button.current?.focus()
                  item.onSelect?.()
                }}
              >
                {content}
              </button>
            )
          })}
        </div>
      ) : null}
    </div>
  )
}
