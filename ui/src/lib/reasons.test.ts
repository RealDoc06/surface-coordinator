import { describe, expect, it } from 'vitest'
import { isCancelled, reasonLabel, reasonText } from './reasons'

describe('reasons', () => {
  it('has words for every reason the coordinator can send', () => {
    for (const reason of [
      'unsupported',
      'timeout',
      'nonzero_exit',
      'invalid_payload',
      'interrupted',
      'error',
      'cancelled',
      'no_user_session',
      'bad_signature',
      'bad_hash',
      'downgrade_refused',
      'download_failed',
      'rolled_back',
    ]) {
      expect(reasonLabel(reason), reason).not.toBe(reason)
    }
  })

  it('shows an unknown reason as it was sent', () => {
    expect(reasonLabel('from_the_future')).toBe('from_the_future')
  })

  it('joins the reason and the message', () => {
    expect(reasonText({ reason: 'download_failed', message: 'HTTP 404' })).toBe(
      'The device could not download the update: HTTP 404',
    )
    expect(reasonText({ reason: null, message: 'already on 1.2.3' })).toBe('already on 1.2.3')
    expect(reasonText({ reason: null, message: null })).toBe('')
  })

  it('recognises a cancelled target', () => {
    expect(isCancelled({ state: 'failed', reason: 'cancelled' })).toBe(true)
    expect(isCancelled({ state: 'failed', reason: 'timeout' })).toBe(false)
    expect(isCancelled({ state: 'succeeded', reason: 'cancelled' })).toBe(false)
  })
})
