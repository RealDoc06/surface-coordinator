// The settings catalogue (`GET /settings-catalogue`): finding entries, editing
// their values and checking them with the rules the coordinator applies.

import type { Setting } from '../api/types'
import { codePointLength } from '../state/store'

export type SettingValue = boolean | number | string

export function findSetting(settings: Setting[] | null | undefined, id: string): Setting | undefined {
  return settings?.find((s) => s.id === id)
}

/** An integer 0..100 in percent: edited with a slider as well as a number box. */
export function isPercent(setting: Setting): boolean {
  const v = setting.value
  return v.type === 'integer' && v.min === 0 && v.max === 100 && ['%', 'percent'].includes(v.unit.trim().toLowerCase())
}

/** What the editor starts with, as the text the input holds. */
export function initialValueText(setting: Setting): string {
  const v = setting.value
  switch (v.type) {
    case 'bool':
      return 'off'
    case 'integer':
      return String(v.min)
    case 'text':
      return ''
  }
}

export type ParsedValue = { value: SettingValue; error?: undefined } | { value?: undefined; error: string }

/**
 * Reads the editor's text into a value of the setting's type. A bool is 'on' or
 * 'off', an integer is a whole number within min..max, text is at most max_len
 * characters.
 */
export function parseSettingValue(setting: Setting, text: string): ParsedValue {
  const v = setting.value
  switch (v.type) {
    case 'bool':
      return { value: text === 'on' }
    case 'integer': {
      const unit = v.unit ? ` ${v.unit}` : ''
      const range = `from ${v.min} to ${v.max}${unit}`
      if (!/^-?\d+$/.test(text.trim())) return { error: `Enter a whole number ${range}.` }
      const n = Number(text.trim())
      if (!Number.isSafeInteger(n) || n < v.min || n > v.max) return { error: `Enter a whole number ${range}.` }
      return { value: n }
    }
    case 'text': {
      const length = codePointLength(text)
      if (length > v.max_len) return { error: `At most ${v.max_len} characters (this is ${length}).` }
      return { value: text }
    }
  }
}

/** The editor text for an existing value ("Run again"); null if it does not fit the setting's type. */
export function valueToText(setting: Setting, value: unknown): string | null {
  switch (setting.value.type) {
    case 'bool':
      return typeof value === 'boolean' ? (value ? 'on' : 'off') : null
    case 'integer':
      return typeof value === 'number' && Number.isFinite(value) ? String(value) : null
    case 'text':
      return typeof value === 'string' ? value : null
  }
}

/** "on", "40 %", "20 minutes", or the text quoted. `setting` may be unknown (catalogue not loaded). */
export function formatSettingValue(setting: Setting | undefined, value: unknown): string {
  if (typeof value === 'boolean') return value ? 'on' : 'off'
  if (typeof value === 'number') {
    const unit = setting?.value.type === 'integer' ? setting.value.unit.trim() : ''
    return unit ? `${value} ${unit}` : String(value)
  }
  if (typeof value === 'string') return `“${value}”`
  return String(value)
}
