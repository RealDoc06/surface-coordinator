// Rules for scripts in the library, as the coordinator applies them.

import type { Script, ScriptInput } from '../api/types'
import { codePointLength } from '../state/store'

export const MAX_SCRIPT_NAME_CHARS = 100
export const MAX_SCRIPT_BYTES = 256 * 1024

export type ScriptErrors = { name?: string; body?: string }

export function validateScript(input: ScriptInput): ScriptErrors {
  const errors: ScriptErrors = {}
  const name = input.name.trim()
  if (name === '') errors.name = 'Give the script a name.'
  else if (codePointLength(name) > MAX_SCRIPT_NAME_CHARS) {
    errors.name = `The name can be at most ${MAX_SCRIPT_NAME_CHARS} characters.`
  }
  if (input.body.trim() === '') errors.body = 'The script is empty.'
  else if (new TextEncoder().encode(input.body).length > MAX_SCRIPT_BYTES) {
    errors.body = `The script can be at most ${MAX_SCRIPT_BYTES / 1024} KiB.`
  }
  return errors
}

/** The name a duplicate starts with. */
export function copyName(name: string): string {
  return `${name} (copy)`
}

export function scriptsByName(scripts: Script[]): Script[] {
  return scripts.slice().sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base', numeric: true }))
}
