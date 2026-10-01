// The things an operator can do to a set of devices: one entry per job kind
// the UI has a form for. The "New job" page, the menus that lead to it and
// "Run again" all read this list.

import type { CreateJob } from '../api/types'

export type ActionKind = CreateJob['kind']

export type Action = {
  kind: ActionKind
  /** The form's route. */
  path: string
  /** Heading of the form and its tab. */
  title: string
  /** Menu entry. */
  menu: string
}

export const ACTIONS: readonly Action[] = [
  { kind: 'script.run', path: '/run', title: 'Run script', menu: 'Run script…' },
  { kind: 'app.launch', path: '/launch', title: 'Launch app', menu: 'Launch app…' },
  { kind: 'setting.apply', path: '/setting', title: 'Apply setting', menu: 'Apply setting…' },
  { kind: 'power', path: '/power', title: 'Power', menu: 'Restart, shut down or sleep…' },
  { kind: 'wifi.join', path: '/wifi', title: 'Move to Wi-Fi', menu: 'Move to Wi-Fi…' },
  { kind: 'client.update', path: '/update', title: 'Update client', menu: 'Update client…' },
]

export function actionFor(kind: ActionKind): Action {
  const found = ACTIONS.find((a) => a.kind === kind)
  if (!found) throw new Error(`no action for ${kind}`)
  return found
}
