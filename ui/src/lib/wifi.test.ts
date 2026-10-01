import { describe, expect, it } from 'vitest'
import { job, target } from '../test/helpers'
import { jobKindLabel, jobPreview, rerunRequest } from './jobKinds'
import {
  asWifiJoin,
  describeNetwork,
  utf8Length,
  validateCoordinatorUrl,
  validatePassword,
  validateSsid,
  validateWifiForm,
  wifiPayload,
} from './wifi'

describe('SSID validation', () => {
  it('requires a name', () => {
    expect(validateSsid('')).toMatch(/Enter the name/)
    expect(validateSsid('Classroom')).toBeNull()
  })

  it('measures bytes, not characters', () => {
    expect(utf8Length('abc')).toBe(3)
    expect(utf8Length('é')).toBe(2)
    expect(utf8Length('✓')).toBe(3)
    expect(utf8Length('\u{1F600}')).toBe(4)

    expect(validateSsid('a'.repeat(32))).toBeNull()
    expect(validateSsid('a'.repeat(33))).toMatch(/at most 32 bytes; this one is 33/)
    // 16 two-byte characters fit, 17 do not, though both are under 32 characters.
    expect(validateSsid('é'.repeat(16))).toBeNull()
    expect(validateSsid('é'.repeat(17))).toMatch(/this one is 34/)
    // 8 emoji are 8 characters but 32 bytes.
    expect(validateSsid('\u{1F600}'.repeat(8))).toBeNull()
    expect(validateSsid('\u{1F600}'.repeat(9))).toMatch(/this one is 36/)
  })
})

describe('password validation', () => {
  it('is optional: empty means an open network', () => {
    expect(validatePassword('')).toBeNull()
  })

  it('takes 8 to 63 characters', () => {
    expect(validatePassword('1234567')).toMatch(/this one is 7/)
    expect(validatePassword('12345678')).toBeNull()
    expect(validatePassword('p'.repeat(63))).toBeNull()
    expect(validatePassword('p'.repeat(64))).toMatch(/this one is 64/)
  })

  it('counts characters, so a few astral ones are still characters', () => {
    expect(validatePassword('\u{1F600}'.repeat(8))).toBeNull()
    expect(validatePassword('\u{1F600}'.repeat(7))).toMatch(/this one is 7/)
  })
})

describe('coordinator address validation', () => {
  it('is optional', () => {
    expect(validateCoordinatorUrl('')).toBeNull()
    expect(validateCoordinatorUrl('   ')).toBeNull()
  })

  it('accepts ws:// and wss:// with a host', () => {
    expect(validateCoordinatorUrl('ws://192.168.137.1:8081')).toBeNull()
    expect(validateCoordinatorUrl('wss://coordinator.example.org')).toBeNull()
    expect(validateCoordinatorUrl('  ws://host  ')).toBeNull()
  })

  it('rejects anything else', () => {
    for (const bad of ['http://host', 'https://host', '192.168.1.1:8081', 'ws://', 'wss:///path', 'ws:/host', 'ws://a b']) {
      expect(validateCoordinatorUrl(bad), bad).toMatch(/ws:\/\/ or wss:\/\//)
    }
  })
})

describe('wifiPayload', () => {
  it('leaves out an empty password and address', () => {
    expect(wifiPayload({ ssid: 'Guest', password: '', coordinatorUrl: '' })).toEqual({ ssid: 'Guest' })
    expect(wifiPayload({ ssid: 'Guest', password: '', coordinatorUrl: '   ' })).toEqual({ ssid: 'Guest' })
  })

  it('sends what was entered, with the address trimmed and its scheme in lower case', () => {
    expect(
      wifiPayload({ ssid: ' Guest ', password: 'secret pass', coordinatorUrl: ' WS://10.0.0.1:8081 ' }),
    ).toEqual({ ssid: ' Guest ', password: 'secret pass', coordinator_url: 'ws://10.0.0.1:8081' })
  })

  it('validates all fields at once', () => {
    expect(validateWifiForm({ ssid: '', password: 'short', coordinatorUrl: 'http://x' })).toEqual({
      ssid: expect.any(String),
      password: expect.any(String),
      coordinatorUrl: expect.any(String),
    })
    expect(validateWifiForm({ ssid: 'ok', password: '', coordinatorUrl: '' })).toEqual({})
  })
})

describe('asWifiJoin', () => {
  it('reads a stored payload and rejects other shapes', () => {
    expect(asWifiJoin({ ssid: 'a', password: '********', coordinator_url: 'ws://h' })).toEqual({
      ssid: 'a',
      password: '********',
      coordinator_url: 'ws://h',
    })
    expect(asWifiJoin({ ssid: 'a' })).toEqual({ ssid: 'a' })
    expect(asWifiJoin({ shell: 'cmd', body: 'x' })).toBeNull()
    expect(asWifiJoin(null)).toBeNull()
  })
})

describe('describeNetwork', () => {
  it('prefers the SSID and keeps the address as secondary', () => {
    expect(describeNetwork({ ssid: 'Lab', ip: '10.1.1.5' })).toEqual({
      primary: 'Lab',
      secondary: '10.1.1.5',
      title: 'Lab (10.1.1.5)',
    })
    expect(describeNetwork({ ssid: 'Lab', ip: null })).toEqual({ primary: 'Lab', secondary: null, title: undefined })
  })

  it('falls back to the address, and to nothing', () => {
    expect(describeNetwork({ ssid: null, ip: '10.1.1.5' }).primary).toBe('10.1.1.5')
    expect(describeNetwork({ ssid: null, ip: null }).primary).toBeNull()
    expect(describeNetwork({ ssid: '', ip: '' }).primary).toBeNull()
    expect(describeNetwork(undefined).primary).toBeNull()
  })
})

describe('wifi.join job kind', () => {
  const wifiJob = job('w1', [target('t1', 'a'), target('t2', 'b'), target('t3', 'a')], {
    kind: 'wifi.join',
    payload: { ssid: 'Classroom', password: '********', coordinator_url: 'ws://10.0.0.1:8081' },
  })

  it('has a label and a one-line preview', () => {
    expect(jobKindLabel('wifi.join')).toBe('Wi-Fi')
    expect(jobPreview(wifiJob)).toBe('Join Wi-Fi Classroom')
  })

  it('previews a payload it cannot read as JSON, like any other kind', () => {
    expect(jobPreview({ kind: 'wifi.join', payload: { nonsense: true } })).toBe('{"nonsense":true}')
  })

  it('runs again by re-asking for the password: it is not prefilled', () => {
    expect(rerunRequest(wifiJob)).toEqual({
      path: '/wifi',
      state: {
        targets: { type: 'devices', device_ids: ['a', 'b'] },
        wifi: { ssid: 'Classroom', coordinator_url: 'ws://10.0.0.1:8081', passwordNeeded: true },
      },
    })
  })

  it('does not ask for a password that an open network never had', () => {
    const open = job('w2', [target('t1', 'a')], { kind: 'wifi.join', payload: { ssid: 'Open' } })
    expect(rerunRequest(open)?.state.wifi).toEqual({ ssid: 'Open' })
  })

  it('keeps sending script jobs to the script composer', () => {
    expect(rerunRequest(job('s1', [target('t1', 'a')]))?.path).toBe('/run')
  })
})
