// Validation and request-building for wifi.join jobs. The limits are the
// coordinator's (docs/api.md): the SSID is measured in bytes, the passphrase
// in characters.

import type { DeviceNetwork, WifiJoin } from '../api/types'

export const SSID_MAX_BYTES = 32
export const PASSWORD_MIN_CHARS = 8
export const PASSWORD_MAX_CHARS = 63
/** What the coordinator returns in place of a stored password. */
export const MASKED_PASSWORD = '********'

const encoder = new TextEncoder()

/** Length in UTF-8 bytes: what an SSID is limited by, not the number of characters. */
export function utf8Length(text: string): number {
  return encoder.encode(text).length
}

export type WifiForm = { ssid: string; password: string; coordinatorUrl: string }
export type WifiErrors = { ssid?: string; password?: string; coordinatorUrl?: string }

export function validateSsid(ssid: string): string | null {
  if (ssid === '') return 'Enter the name (SSID) of the network.'
  const bytes = utf8Length(ssid)
  if (bytes > SSID_MAX_BYTES) {
    return `A network name is at most ${SSID_MAX_BYTES} bytes; this one is ${bytes} (characters outside plain ASCII take 2 to 4 bytes each).`
  }
  return null
}

/** Optional: empty means an open network. */
export function validatePassword(password: string): string | null {
  if (password === '') return null
  const chars = [...password].length
  if (chars < PASSWORD_MIN_CHARS || chars > PASSWORD_MAX_CHARS) {
    return `A Wi-Fi password is ${PASSWORD_MIN_CHARS} to ${PASSWORD_MAX_CHARS} characters; this one is ${chars}. Leave it empty for an open network.`
  }
  return null
}

/** Optional: empty keeps the coordinator the devices use now. */
export function validateCoordinatorUrl(url: string): string | null {
  const trimmed = url.trim()
  if (trimmed === '') return null
  if (!/^wss?:\/\/[^\s/]+/i.test(trimmed) || /\s/.test(trimmed)) {
    return 'The address has to start with ws:// or wss:// and name a host, for example ws://192.168.137.1:8081.'
  }
  return null
}

export function validateWifiForm(form: WifiForm): WifiErrors {
  const errors: WifiErrors = {}
  const ssid = validateSsid(form.ssid)
  if (ssid) errors.ssid = ssid
  const password = validatePassword(form.password)
  if (password) errors.password = password
  const url = validateCoordinatorUrl(form.coordinatorUrl)
  if (url) errors.coordinatorUrl = url
  return errors
}

/** The job payload for a form that passed validation. Empty optional fields are left out. */
export function wifiPayload(form: WifiForm): WifiJoin {
  const payload: WifiJoin = { ssid: form.ssid }
  if (form.password !== '') payload.password = form.password
  const url = form.coordinatorUrl.trim()
  if (url !== '') payload.coordinator_url = url.replace(/^wss?:\/\//i, (scheme) => scheme.toLowerCase())
  return payload
}

/** The stored payload of a wifi.join job, or null if it does not look like one. */
export function asWifiJoin(payload: unknown): WifiJoin | null {
  if (typeof payload !== 'object' || payload === null) return null
  const p = payload as Record<string, unknown>
  if (typeof p.ssid !== 'string') return null
  const wifi: WifiJoin = { ssid: p.ssid }
  if (typeof p.password === 'string') wifi.password = p.password
  if (typeof p.coordinator_url === 'string') wifi.coordinator_url = p.coordinator_url
  return wifi
}

/** How to show what a device reported about its network. */
export function describeNetwork(network: DeviceNetwork | undefined): {
  /** The SSID, or the address when there is no SSID (a wired device), or null. */
  primary: string | null
  /** The IP address when it adds to the SSID. */
  secondary: string | null
  title: string | undefined
} {
  const ssid = network?.ssid || null
  const ip = network?.ip || null
  return {
    primary: ssid ?? ip,
    secondary: ssid ? ip : null,
    title: ssid && ip ? `${ssid} (${ip})` : undefined,
  }
}
