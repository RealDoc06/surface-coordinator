// Validation for app.launch: the same limits the coordinator enforces.

import type { AppLaunch } from '../api/types'
import { codePointLength } from '../state/store'

export const MAX_PATH_CHARS = 1024
export const MAX_ARGUMENTS = 64
export const MAX_ARGUMENT_CHARS = 1024

export type LaunchErrors = {
  path?: string
  /** One message per argument row that is too long, by row index. */
  arguments?: Record<number, string>
  /** Too many arguments. */
  count?: string
}

export type LaunchForm = { path: string; arguments: string[] }

/** The payload for the form (blank argument rows dropped), and what is wrong with it. */
export function validateLaunch(form: LaunchForm): { payload: AppLaunch | null; errors: LaunchErrors } {
  const errors: LaunchErrors = {}
  const path = form.path.trim()
  if (path === '') errors.path = 'Enter the path of the program to start.'
  else if (codePointLength(path) > MAX_PATH_CHARS) {
    errors.path = `The path can be at most ${MAX_PATH_CHARS} characters (it is ${codePointLength(path)}).`
  }

  const args = form.arguments.map((value, row) => ({ value, row })).filter((a) => a.value.trim() !== '')
  if (args.length > MAX_ARGUMENTS) errors.count = `At most ${MAX_ARGUMENTS} arguments (there are ${args.length}).`
  for (const { value, row } of args) {
    if (codePointLength(value) > MAX_ARGUMENT_CHARS) {
      errors.arguments = {
        ...errors.arguments,
        [row]: `An argument can be at most ${MAX_ARGUMENT_CHARS} characters (this one is ${codePointLength(value)}).`,
      }
    }
  }
  if (Object.keys(errors).length > 0) return { payload: null, errors }
  const payload: AppLaunch = { path }
  if (args.length > 0) payload.arguments = args.map((a) => a.value)
  return { payload, errors }
}
