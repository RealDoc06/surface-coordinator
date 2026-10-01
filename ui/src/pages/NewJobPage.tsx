import { useMemo, useState, type ComponentProps } from 'react'
import { Link, NavLink, useLocation, useNavigate } from 'react-router-dom'
import type { Job, TargetSpec } from '../api/types'
import { ClientUpdateForm } from '../components/ClientUpdateForm'
import { LaunchAppForm } from '../components/LaunchAppForm'
import { PowerForm } from '../components/PowerForm'
import { ScriptComposer } from '../components/ScriptComposer'
import { SettingForm } from '../components/SettingForm'
import { WifiJoinForm } from '../components/WifiJoinForm'
import { ACTIONS, actionFor, type ActionKind } from '../lib/actions'
import { parseComposeRequest } from '../lib/targets'
import { useStore } from '../state/StoreProvider'
import { sortedDevices } from '../state/store'

const ALL_LABELS: Partial<Record<ActionKind, string>> = {
  'script.run': 'Run on all devices instead',
  'wifi.join': 'Move all devices instead',
}

/**
 * One page for every kind of job: the operator chooses the action (tabs) for
 * the targets (chosen on the devices page, a group, or everything) and fills
 * in that action's form. The targets survive switching the action.
 */
export function NewJobPage({ kind }: { kind: ActionKind }) {
  const { state, dispatch, reload } = useStore()
  const navigate = useNavigate()
  const location = useLocation()
  const request = useMemo(() => parseComposeRequest(location.state), [location.state])
  const [targets, setTargets] = useState<TargetSpec | null>(request.targets ?? null)
  const devices = useMemo(() => sortedDevices(state), [state.devices])
  const groups = state.groups
  const action = actionFor(kind)

  function onCreated(job: Job) {
    // Show it at once; the job.created event carries the same job.
    dispatch({ type: 'event', event: { type: 'job.created', job } })
    navigate(`/jobs/${encodeURIComponent(job.id)}`)
  }

  const selectedIds = targets?.type === 'devices' ? targets.device_ids : []

  const targetActions = (
    <>
      <Link to="/" state={{ selected: selectedIds }}>
        {selectedIds.length > 0 ? 'Change selection' : 'Select devices'}
      </Link>
      {targets?.type !== 'all' ? (
        <button type="button" className="link" onClick={() => setTargets({ type: 'all' })}>
          {ALL_LABELS[kind] ?? 'Use all devices instead'}
        </button>
      ) : null}
      {groups.length > 0 ? (
        <label className="field-inline">
          <span>Use a group</span>
          <select
            value={targets?.type === 'group' ? targets.group_id : ''}
            onChange={(e) => {
              if (e.target.value !== '') setTargets({ type: 'group', group_id: e.target.value })
            }}
          >
            <option value="">Choose a group…</option>
            {groups.map((g) => (
              <option key={g.id} value={g.id}>
                {g.name} ({g.device_ids.length})
              </option>
            ))}
          </select>
        </label>
      ) : null}
    </>
  )

  const common = {
    targets,
    devices,
    groups,
    onCreated,
    onCancel: () => navigate(-1),
    targetActions,
  }

  return (
    <>
      <div className="page-head">
        <h1>{action.title}</h1>
      </div>
      <nav className="tabs" aria-label="Action">
        {ACTIONS.map((a) => (
          <NavLink key={a.kind} to={a.path} state={{ targets }} replace end>
            {a.title}
          </NavLink>
        ))}
      </nav>
      {state.devicesLoaded ? (
        <ActionForm kind={kind} common={common} request={request} catalogue={state.catalogue} reload={reload} />
      ) : (
        <p className="muted">Loading devices&hellip;</p>
      )}
    </>
  )
}

function ActionForm({
  kind,
  common,
  request,
  catalogue,
  reload,
}: {
  kind: ActionKind
  common: Omit<ComponentProps<typeof LaunchAppForm>, 'initial'>
  request: ReturnType<typeof parseComposeRequest>
  catalogue: ComponentProps<typeof SettingForm>['catalogue']
  reload: () => void
}) {
  switch (kind) {
    case 'script.run':
      return <ScriptComposer key={kind} {...common} initial={request.payload} />
    case 'wifi.join':
      return <WifiJoinForm key={kind} {...common} initial={request.wifi} />
    case 'app.launch':
      return <LaunchAppForm key={kind} {...common} initial={request.launch} />
    case 'setting.apply':
      return (
        <SettingForm
          key={catalogue ? 'ready' : 'waiting'}
          {...common}
          catalogue={catalogue}
          initial={request.setting}
          onRetryCatalogue={reload}
        />
      )
    case 'power':
      return <PowerForm key={kind} {...common} initial={request.power} />
    case 'client.update':
      return <ClientUpdateForm key={kind} {...common} initial={request.update} />
  }
}
