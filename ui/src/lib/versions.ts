// Client versions are compared numerically, part by part: 1.2.10 is newer than
// 1.2.9. Anything that does not look like a version is "unknown" and is never
// treated as older (or newer) than anything, so the UI never offers a
// downgrade it cannot be sure about.

/** The numeric parts of "1.2.3" (a leading "v" and a "-pre" or "+build" suffix are ignored), or null. */
export function parseVersion(text: string): number[] | null {
  const match = /^v?(\d+(?:\.\d+){0,3})(?:[-+][0-9A-Za-z.+-]*)?$/.exec(text.trim())
  if (!match || match[1] === undefined) return null
  const parts = match[1].split('.').map(Number)
  return parts.every((n) => Number.isSafeInteger(n)) ? parts : null
}

/** Negative if a is older than b, positive if newer, 0 if equal; null if either is not a version. */
export function compareVersions(a: string, b: string): number | null {
  const left = parseVersion(a)
  const right = parseVersion(b)
  if (!left || !right) return null
  for (let i = 0; i < Math.max(left.length, right.length); i++) {
    const diff = (left[i] ?? 0) - (right[i] ?? 0)
    if (diff !== 0) return diff
  }
  return 0
}

/** True only when both are versions and `version` is strictly older than `than`. */
export function isOlder(version: string, than: string): boolean {
  const cmp = compareVersions(version, than)
  return cmp !== null && cmp < 0
}
