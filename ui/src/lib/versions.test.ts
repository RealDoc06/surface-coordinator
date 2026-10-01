import { describe, expect, it } from 'vitest'
import { compareVersions, isOlder, parseVersion } from './versions'

describe('parseVersion', () => {
  it('reads dotted numbers', () => {
    expect(parseVersion('1.2.3')).toEqual([1, 2, 3])
    expect(parseVersion('v0.10.0')).toEqual([0, 10, 0])
    expect(parseVersion(' 2.0 ')).toEqual([2, 0])
  })

  it('ignores a pre-release or build suffix', () => {
    expect(parseVersion('1.2.3-beta.1')).toEqual([1, 2, 3])
    expect(parseVersion('1.2.3+abc')).toEqual([1, 2, 3])
  })

  it('rejects what is not a version', () => {
    for (const text of ['', 'latest', '1..2', '1.2.x', 'a.b.c', '1.2.3.4.5', '99999999999999999999.0']) {
      expect(parseVersion(text), text).toBeNull()
    }
  })
})

describe('compareVersions', () => {
  it('compares numerically, not as text', () => {
    expect(compareVersions('1.2.10', '1.2.9')).toBeGreaterThan(0)
    expect(compareVersions('1.2.9', '1.2.10')).toBeLessThan(0)
    expect(compareVersions('1.10.0', '1.9.9')).toBeGreaterThan(0)
    expect(compareVersions('2.0.0', '1.99.99')).toBeGreaterThan(0)
  })

  it('treats missing parts as zero', () => {
    expect(compareVersions('1.2', '1.2.0')).toBe(0)
    expect(compareVersions('1.2', '1.2.1')).toBeLessThan(0)
  })

  it('is null when either side is unparseable', () => {
    expect(compareVersions('dev', '1.0.0')).toBeNull()
    expect(compareVersions('1.0.0', 'dev')).toBeNull()
  })
})

describe('isOlder', () => {
  it('is true only for a strictly older, parseable version', () => {
    expect(isOlder('1.2.9', '1.2.10')).toBe(true)
    expect(isOlder('1.2.10', '1.2.9')).toBe(false)
    expect(isOlder('1.2.3', '1.2.3')).toBe(false)
  })

  it('is never true for unparseable versions', () => {
    expect(isOlder('dev', '1.0.0')).toBe(false)
    expect(isOlder('0.0.1', 'dev')).toBe(false)
  })
})
