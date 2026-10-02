import { useEffect, useMemo, useState } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'
import { api, errorMessage } from '../api/client'
import type { ManagerWifi, TargetSpec } from '../api/types'
import { DeviceList } from '../components/DeviceList'
import type { MembershipOp } from '../components/GroupMembershipDialog'
import { ConfirmDialog } from '../components/Modal'
import { useSession } from '../session/SessionProvider'
import { actionFor, type ActionKind } from '../lib/actions'
import { plural, useNow } from '../lib/format'
import type { ComposeRequest } from '../lib/targets'
import { useStore } from '../state/StoreProvider'
import { keyLabels, sortedDevices } from '../state/store'

function initialSelection(state: unknown): string[] | undefined {
  if (typeof state !== 'object' || state === null) return undefined
  const selected = (state as { selected?: unknown }).selected
  return Array.isArray(selected) ? selected.filter((v): v is string => typeof v === 'string') : undefined
}

export function DevicesPage() {
  const { state, reloadGroups } = useStore()
  const navigate = useNavigate()
  const location = useLocation()
  const now = useNow(15_000)

  const devices = useMemo(() => sortedDevices(state), [state.devices])
  const labels = useMemo(() => keyLabels(state), [state.keys])
  const online = devices.filter((d) => d.online).length

  // Inside the manager: the Wi-Fi network its computer is on, which devices
  // can be moved to without anyone typing its password.
  const inManager = useSession().capabilities.hotspot
  const [localWifi, setLocalWifi] = useState<ManagerWifi | null>(null)
  const [moveLocal, setMoveLocal] = useState<TargetSpec | null>(null)
  const [moving, setMoving] = useState(false)
  const [moveError, setMoveError] = useState<string | null>(null)
  useEffect(() => {
    if (!inManager) return
    let current = true
    api
      .getManagerWifi()
      .then((wifi) => current && setLocalWifi(wifi))
      .catch(() => current && setLocalWifi(null))
    return () => {
      current = false
    }
  }, [inManager, state.syncEpoch])

  async function confirmMoveLocal() {
    if (!moveLocal) return
    setMoving(true)
    setMoveError(null)
    try {
      const { job } = await api.moveToManagerWifi(moveLocal)
      navigate(`/jobs/${job.id}`)
    } catch (err) {
      setMoveError(errorMessage(err))
      setMoving(false)
    }
  }

  function onRun(targets: TargetSpec) {
    const request: ComposeRequest = { targets }
    navigate('/run', { state: request })
  }

  function onMove(targets: TargetSpec) {
    const request: ComposeRequest = { targets }
    navigate('/wifi', { state: request })
  }

  function onAction(kind: ActionKind, targets: TargetSpec) {
    const request: ComposeRequest = { targets }
    navigate(actionFor(kind).path, { state: request })
  }

  function onLibrary(targets: TargetSpec) {
    const request: ComposeRequest = { targets }
    navigate('/scripts', { state: request })
  }

  function onRunGroup(groupId: string) {
    const request: ComposeRequest = { targets: { type: 'group', group_id: groupId } }
    navigate('/run', { state: request })
  }

  /**
   * PUT replaces a group's whole membership, so merge into (or reduce) the
   * membership as it is now, not as this page last saw it.
   */
  async function onGroupMembers(op: MembershipOp, groupId: string, deviceIds: string[]) {
    const { groups } = await api.listGroups()
    const group = groups.find((g) => g.id === groupId)
    if (!group) throw new Error('That group no longer exists.')
    const changed = new Set(deviceIds)
    const members =
      op === 'add'
        ? [...new Set([...group.device_ids, ...deviceIds])]
        : group.device_ids.filter((id) => !changed.has(id))
    await api.setGroupMembers(groupId, members)
    reloadGroups()
  }

  return (
    <>
      <div className="page-head">
        <h1>Devices</h1>
        {state.devicesLoaded && devices.length > 0 ? (
          <span className="muted">
            {plural(devices.length, 'device')}, {online} online
          </span>
        ) : null}
      </div>
      {state.devicesLoaded ? (
        <DeviceList
          devices={devices}
          keyLabels={labels}
          now={now}
          initialSelected={initialSelection(location.state)}
          groups={state.groups}
          onRun={onRun}
          onMove={onMove}
          localWifi={localWifi?.available ? (localWifi.ssid ?? undefined) : undefined}
          onMoveLocal={(targets) => {
            setMoveError(null)
            setMoveLocal(targets)
          }}
          onAction={onAction}
          onLibrary={onLibrary}
          onRunGroup={onRunGroup}
          onGroupMembers={onGroupMembers}
        />
      ) : (
        <p className="muted">Loading devices&hellip;</p>
      )}
      {moveLocal && localWifi ? (
        <ConfirmDialog
          title={`Move to ${localWifi.ssid}`}
          confirmLabel={moving ? 'Moving…' : 'Move devices'}
          busy={moving}
          error={moveError}
          onConfirm={confirmMoveLocal}
          onCancel={() => setMoveLocal(null)}
        >
          <p>
            {moveLocal.type === 'devices' ? plural(moveLocal.device_ids.length, 'device') : 'The devices'} will
            leave their current network and join <strong>{localWifi.ssid}</strong>, the Wi-Fi this computer is on. The
            saved password is used; nothing needs typing.
          </p>
          {localWifi.warning ? (
            <p className="error" role="alert">
              {localWifi.warning}
            </p>
          ) : null}
          {localWifi.coordinator === 'online' ? (
            <p className="muted">
              They will connect to the online coordinator at <code>{localWifi.coordinator_url}</code> from then on and
              leave this local one; this job is marked as succeeded once they have been gone for two minutes.
            </p>
          ) : (
            <p className="muted">
              They will reach this coordinator at <code>{localWifi.coordinator_url}</code> from then on. A device that
              cannot join comes back to the hotspot and reports the failure.
            </p>
          )}
        </ConfirmDialog>
      ) : null}
    </>
  )
}
