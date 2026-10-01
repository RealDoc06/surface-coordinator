// Why a target ended the way it did, in words, in one place. The coordinator
// sends a machine-readable `reason` (and sometimes a `message` with detail).

import type { Target } from '../api/types'

export const REASONS: Record<string, string> = {
  unsupported: 'Not supported by this client',
  timeout: 'Timed out',
  nonzero_exit: 'Non-zero exit code',
  invalid_payload: 'Client rejected the job parameters',
  interrupted: 'Interrupted',
  error: 'Error',
  cancelled: 'Cancelled',
  no_user_session: 'Nobody is logged in on the device, so there was no user session to run it in',
  bad_signature: 'The update’s signature did not verify, so it was not installed',
  bad_hash: 'The downloaded update did not match its checksum, so it was not installed',
  downgrade_refused: 'The device refused to install an older version (downgrade not allowed)',
  download_failed: 'The device could not download the update',
  rolled_back: 'The new version did not start properly; the device went back to its previous version',
}

/** The wording for a reason code; unknown codes (a newer coordinator) show as sent. */
export function reasonLabel(reason: string): string {
  return REASONS[reason] ?? reason
}

export function reasonText(target: Pick<Target, 'reason' | 'message'>): string {
  const reason = target.reason ? reasonLabel(target.reason) : ''
  if (reason && target.message) return `${reason}: ${target.message}`
  return reason || target.message || ''
}

/** A target stopped by "Cancel job": shown neutrally, not as a failure. */
export function isCancelled(target: Pick<Target, 'state' | 'reason'>): boolean {
  return target.state === 'failed' && target.reason === 'cancelled'
}
