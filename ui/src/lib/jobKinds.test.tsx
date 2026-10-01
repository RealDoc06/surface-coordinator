import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import type { Setting } from '../api/types'
import { job, target } from '../test/helpers'
import { JobPayload, jobKindLabel, jobPreview, rerunRequest } from './jobKinds'

const volume: Setting = {
  id: 'audio.volume',
  label: 'Volume',
  description: '',
  scope: 'user',
  value: { type: 'integer', min: 0, max: 100, unit: '%' },
}

const j = (kind: string, payload: unknown) => job('j1', [target('t1', 'a'), target('t2', 'b')], { kind, payload })

describe('labels and previews', () => {
  it('labels the new kinds', () => {
    expect(jobKindLabel('app.launch')).toBe('Launch app')
    expect(jobKindLabel('setting.apply')).toBe('Setting')
    expect(jobKindLabel('power')).toBe('Power')
    expect(jobKindLabel('client.update')).toBe('Client update')
    expect(jobKindLabel('from.the.future')).toBe('from.the.future')
  })

  it('previews each kind in one line', () => {
    expect(jobPreview(j('app.launch', { path: 'C:\\Apps\\app.exe', arguments: ['-x'] }))).toBe('Launch C:\\Apps\\app.exe')
    expect(jobPreview(j('power', { action: 'restart' }))).toBe('Restart')
    expect(jobPreview(j('power', { action: 'shutdown' }))).toBe('Shut down')
    expect(jobPreview(j('power', { action: 'sleep' }))).toBe('Sleep')
    expect(jobPreview(j('client.update', { version: '1.2.3' }))).toBe('Update client to 1.2.3')
    expect(jobPreview(j('client.update', { version: '1.0.0', downgrade: true }))).toBe(
      'Update client to 1.0.0 (downgrade allowed)',
    )
  })

  it('uses the catalogue label for a setting when loaded, the id otherwise', () => {
    const apply = j('setting.apply', { setting: 'audio.volume', value: 40 })
    expect(jobPreview(apply, { settings: [volume] })).toBe('Set Volume to 40 %')
    expect(jobPreview(apply)).toBe('Set audio.volume to 40')
    expect(jobPreview(apply, { settings: null })).toBe('Set audio.volume to 40')
    expect(jobPreview(j('setting.apply', { setting: 'ui.dark', value: true }))).toBe('Set ui.dark to on')
  })

  it('falls back to the payload for a malformed payload or an unknown kind', () => {
    expect(jobPreview(j('power', { action: 'explode' }))).toBe('{"action":"explode"}')
    expect(jobPreview(j('from.the.future', { x: 1 }))).toBe('{"x":1}')
  })
})

describe('payload views', () => {
  it('shows the launch arguments as a list', () => {
    render(<JobPayload job={j('app.launch', { path: 'C:\\a.exe', arguments: ['-x', 'two words'] })} />)
    expect(screen.getByText('C:\\a.exe')).toBeInTheDocument()
    expect(screen.getAllByRole('listitem').map((li) => li.textContent)).toEqual(['-x', 'two words'])
  })

  it('shows the setting by label with its id, and the value with its unit', () => {
    render(<JobPayload job={j('setting.apply', { setting: 'audio.volume', value: 40 })} context={{ settings: [volume] }} />)
    expect(screen.getByText('Volume')).toBeInTheDocument()
    expect(screen.getByText('audio.volume')).toBeInTheDocument()
    expect(screen.getByText('40 %')).toBeInTheDocument()
  })

  it('shows power and client update parameters', () => {
    const { unmount } = render(<JobPayload job={j('power', { action: 'shutdown' })} />)
    expect(screen.getByText('Shut down')).toBeInTheDocument()
    unmount()
    render(<JobPayload job={j('client.update', { version: '1.2.3', downgrade: true })} />)
    expect(screen.getByText('1.2.3')).toBeInTheDocument()
    expect(screen.getByText('allowed')).toBeInTheDocument()
  })

  it('shows an unknown kind as JSON', () => {
    render(<JobPayload job={j('from.the.future', { x: 1 })} />)
    expect(screen.getByLabelText('Payload')).toHaveTextContent('"x": 1')
  })
})

describe('run again', () => {
  it('sends each kind back to its form with the values and the devices', () => {
    const devices = { type: 'devices', device_ids: ['a', 'b'] }
    expect(rerunRequest(j('app.launch', { path: 'C:\\a.exe', arguments: ['-x'] }))).toEqual({
      path: '/launch',
      state: { launch: { path: 'C:\\a.exe', arguments: ['-x'] }, targets: devices },
    })
    expect(rerunRequest(j('app.launch', { path: 'C:\\a.exe' }))?.state.launch).toEqual({ path: 'C:\\a.exe', arguments: [] })
    expect(rerunRequest(j('power', { action: 'sleep' }))).toEqual({
      path: '/power',
      state: { power: { action: 'sleep' }, targets: devices },
    })
    expect(rerunRequest(j('client.update', { version: '1.2.3' }))).toEqual({
      path: '/update',
      state: { update: { version: '1.2.3' }, targets: devices },
    })
    expect(rerunRequest(j('client.update', { version: '1.0.0', downgrade: true }))?.state.update).toEqual({
      version: '1.0.0',
      downgrade: true,
    })
    expect(rerunRequest(j('setting.apply', { setting: 'audio.volume', value: 40 }))).toEqual({
      path: '/setting',
      state: { setting: { setting: 'audio.volume', value: 40 }, targets: devices },
    })
  })

  it('drops a setting value that no longer fits the catalogue', () => {
    const request = rerunRequest(j('setting.apply', { setting: 'audio.volume', value: true }), { settings: [volume] })
    expect(request?.state.setting).toEqual({ setting: 'audio.volume' })
  })

  it('has none for an unknown kind or a malformed payload', () => {
    expect(rerunRequest(j('from.the.future', {}))).toBeNull()
    expect(rerunRequest(j('power', { action: 'explode' }))).toBeNull()
  })
})
