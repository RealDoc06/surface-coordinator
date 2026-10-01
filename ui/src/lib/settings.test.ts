import { describe, expect, it } from 'vitest'
import type { Setting } from '../api/types'
import { findSetting, formatSettingValue, initialValueText, isPercent, parseSettingValue, valueToText } from './settings'

const volume: Setting = {
  id: 'audio.volume',
  label: 'Volume',
  description: 'Master volume',
  scope: 'user',
  value: { type: 'integer', min: 0, max: 100, unit: '%' },
}
const timeout: Setting = {
  id: 'power.sleep_timeout',
  label: 'Sleep timeout',
  description: '',
  scope: 'machine',
  value: { type: 'integer', min: 1, max: 720, unit: 'minutes' },
}
const dark: Setting = { id: 'ui.dark', label: 'Dark mode', description: '', scope: 'user', value: { type: 'bool' } }
const wallpaper: Setting = {
  id: 'ui.wallpaper',
  label: 'Wallpaper',
  description: '',
  scope: 'user',
  value: { type: 'text', max_len: 10 },
}

describe('parseSettingValue', () => {
  it('reads a bool from on/off', () => {
    expect(parseSettingValue(dark, 'on')).toEqual({ value: true })
    expect(parseSettingValue(dark, 'off')).toEqual({ value: false })
  })

  it('accepts whole numbers within min..max, including the ends', () => {
    expect(parseSettingValue(volume, '0')).toEqual({ value: 0 })
    expect(parseSettingValue(volume, ' 40 ')).toEqual({ value: 40 })
    expect(parseSettingValue(volume, '100')).toEqual({ value: 100 })
  })

  it('rejects numbers out of range, fractions and text, naming the range and unit', () => {
    for (const text of ['101', '-1', '3.5', 'abc', '', '1e2']) {
      const result = parseSettingValue(volume, text)
      expect(result.error, text).toBe('Enter a whole number from 0 to 100 %.')
    }
    expect(parseSettingValue(timeout, '0').error).toBe('Enter a whole number from 1 to 720 minutes.')
  })

  it('limits text to max_len characters', () => {
    expect(parseSettingValue(wallpaper, 'a'.repeat(10))).toEqual({ value: 'a'.repeat(10) })
    expect(parseSettingValue(wallpaper, 'a'.repeat(11)).error).toBe('At most 10 characters (this is 11).')
    expect(parseSettingValue(wallpaper, '\u{1F600}'.repeat(10)).error).toBeUndefined()
  })
})

describe('helpers', () => {
  it('knows a percent setting', () => {
    expect(isPercent(volume)).toBe(true)
    expect(isPercent(timeout)).toBe(false)
    expect(isPercent(dark)).toBe(false)
  })

  it('starts from off, the minimum, or empty', () => {
    expect(initialValueText(dark)).toBe('off')
    expect(initialValueText(timeout)).toBe('1')
    expect(initialValueText(wallpaper)).toBe('')
  })

  it('turns a value into editor text only when it fits the type', () => {
    expect(valueToText(dark, true)).toBe('on')
    expect(valueToText(dark, 'on')).toBeNull()
    expect(valueToText(volume, 40)).toBe('40')
    expect(valueToText(volume, '40')).toBeNull()
    expect(valueToText(wallpaper, 'x')).toBe('x')
  })

  it('formats values for people', () => {
    expect(formatSettingValue(volume, 40)).toBe('40 %')
    expect(formatSettingValue(timeout, 20)).toBe('20 minutes')
    expect(formatSettingValue(undefined, 40)).toBe('40')
    expect(formatSettingValue(dark, true)).toBe('on')
    expect(formatSettingValue(wallpaper, 'C:\\a.png')).toBe('\u201cC:\\a.png\u201d')
  })

  it('finds a setting by id', () => {
    expect(findSetting([volume, dark], 'ui.dark')).toBe(dark)
    expect(findSetting(null, 'ui.dark')).toBeUndefined()
  })
})
