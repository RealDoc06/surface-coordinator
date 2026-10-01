import { codePointLength } from '../state/store'
import type { ComposeRequest } from './targets'

export const MAX_GROUP_NAME_CHARS = 64

/** The complaint about a group name, or null if it is acceptable (1..64 characters after trimming). */
export function validateGroupName(name: string): string | null {
  const trimmed = name.trim()
  if (trimmed === '') return 'Give the group a name.'
  if (codePointLength(trimmed) > MAX_GROUP_NAME_CHARS) return `Group names are at most ${MAX_GROUP_NAME_CHARS} characters.`
  return null
}

/** Where "Run on group…" goes: the script composer with the group as its target. */
export function runOnGroup(groupId: string): { path: string; state: ComposeRequest } {
  return { path: '/run', state: { targets: { type: 'group', group_id: groupId } } }
}
