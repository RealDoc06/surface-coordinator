import { describe, expect, it } from 'vitest'
import { device } from '../test/helpers'
import { noTargetsMessage, parseComposeRequest, parseTargetSpec, requestTargets, resolveTargets } from './targets'

const devices = [
  device('a', { hostname: 'A', online: true }),
  device('b', { hostname: 'B', online: false }),
  device('c', { hostname: 'C', online: true }),
]
const group = { id: 'g1', name: 'Lab', created_at: 1, device_ids: ['a', 'b', 'gone'] }

describe('resolveTargets', () => {
  it('resolves a group to its members that exist', () => {
    const resolved = resolveTargets({ type: 'group', group_id: 'g1' }, devices, [group])
    expect(resolved.devices.map((d) => d.id)).toEqual(['a', 'b'])
    expect(resolved.offline).toBe(1)
    expect(resolved.group).toBe(group)
  })

  it('reports a group that no longer exists', () => {
    const resolved = resolveTargets({ type: 'group', group_id: 'nope' }, devices, [group])
    expect(resolved.devices).toEqual([])
    expect(resolved.group).toBeNull()
  })

  it('still resolves all and devices', () => {
    expect(resolveTargets({ type: 'all' }, devices).devices).toHaveLength(3)
    const some = resolveTargets({ type: 'devices', device_ids: ['c', 'zzz'] }, devices)
    expect(some.devices.map((d) => d.id)).toEqual(['c'])
    expect(some.missing).toEqual(['zzz'])
  })
})

describe('requestTargets', () => {
  it('sends a group as the group, not as its members', () => {
    const spec = { type: 'group' as const, group_id: 'g1' }
    expect(requestTargets(spec, resolveTargets(spec, devices, [group]))).toEqual(spec)
  })

  it('drops devices that no longer exist from a device list', () => {
    const spec = { type: 'devices' as const, device_ids: ['a', 'zzz'] }
    expect(requestTargets(spec, resolveTargets(spec, devices))).toEqual({ type: 'devices', device_ids: ['a'] })
  })
})

describe('noTargetsMessage', () => {
  it('says what is missing for each kind of target', () => {
    expect(noTargetsMessage({ type: 'all' }, 'run on')).toBe('There are no devices to run on yet.')
    expect(noTargetsMessage({ type: 'group', group_id: 'g' }, 'run on')).toMatch(/no devices/)
    expect(noTargetsMessage(null, 'move')).toBe('Select at least one device to move.')
  })
})

describe('parseTargetSpec', () => {
  it('reads the group variant and rejects a bad one', () => {
    expect(parseTargetSpec({ type: 'group', group_id: 'g1' })).toEqual({ type: 'group', group_id: 'g1' })
    expect(parseTargetSpec({ type: 'group' })).toBeNull()
    expect(parseTargetSpec({ type: 'group', group_id: '' })).toBeNull()
    expect(parseTargetSpec({ type: 'mystery' })).toBeNull()
  })
})

describe('parseComposeRequest', () => {
  it('reads every prefill defensively', () => {
    const request = parseComposeRequest({
      targets: { type: 'group', group_id: 'g1' },
      launch: { path: 'C:\\a.exe', arguments: ['-x', 3] },
      setting: { setting: 'audio.volume', value: 40 },
      power: { action: 'sleep' },
      update: { version: '1.2.3', downgrade: true },
    })
    expect(request).toEqual({
      targets: { type: 'group', group_id: 'g1' },
      launch: { path: 'C:\\a.exe' },
      setting: { setting: 'audio.volume', value: 40 },
      power: { action: 'sleep' },
      update: { version: '1.2.3', downgrade: true },
    })
  })

  it('ignores junk', () => {
    expect(parseComposeRequest(null)).toEqual({})
    expect(parseComposeRequest('x')).toEqual({})
    expect(parseComposeRequest({ power: { action: 'explode' }, setting: { value: {} } })).toEqual({ power: {}, setting: {} })
  })
})
