import { useEffect, useMemo, useState } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'
import { api, ApiError, errorMessage } from '../api/client'
import type { ManagerWifi, TargetSpec } from '../api/types'
import { DeviceList } from '../components/DeviceList'
import type { MembershipOp } from '../components/GroupMembershipDialog'
import { ConfirmDialog } from '../components/Modal'
import { ErrorNote } from '../components/bits'
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
  const { state, loadError, reload, reloadGroups } = useStore()
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
  const [moveLocal, setMoveLocal] = useState<{ targets: TargetSpec; wifi: ManagerWifi } | null>(null)
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
    const wifi = moveLocal?.wifi
    if (!moveLocal || !wifi?.available || !wifi.ssid || !wifi.coordinator_url) return
    setMoving(true)
    setMoveError(null)
    try {
      const { job } = await api.moveToManagerWifi(moveLocal.targets, wifi.ssid, wifi.coordinator_url)
      navigate(`/jobs/${job.id}`)
    } catch (err) {
      setMoveError(errorMessage(err))
      if (err instanceof ApiError && (err.code === 'wifi_changed' || err.code === 'wifi_unavailable')) {
        try {
          const current = await api.getManagerWifi()
          setLocalWifi(current)
          if (current.available && current.ssid && current.coordinator_url) {
            setMoveLocal((previous) => previous ? { ...previous, wifi: current } : null)
            setMoveError('The destination changed. Review the updated network and coordinator, then confirm again.')
          } else {
            setMoveLocal(null)
            setMoveError(current.reason ?? 'The manager Wi-Fi is no longer available.')
          }
        } catch {
          // Keep the reviewed destination. The server rejects it if it is stale.
        }
      }
    } finally {
      setMoving(false)
    }
  }

  const reviewedWifi = moveLocal?.wifi

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
            {plural(devices.length, 'device')}, {online} online, {devices.length - online} offline
          </span>
        ) : null}
        <span className="toolbar-spacer" />
        <button type="button" onClick={reload} disabled={state.baseBuffer !== null}>
          {state.baseBuffer !== null ? 'Refreshing…' : 'Refresh'}
        </button>
      </div>
      {!moveLocal ? <ErrorNote message={moveError} /> : null}
      {state.devicesLoaded ? (
        <DeviceList
          devices={devices}
          keyLabels={labels}
          now={now}
          live={state.connection === 'open'}
          initialSelected={initialSelection(location.state)}
          groups={state.groups}
          onRun={onRun}
          onMove={onMove}
          localWifi={localWifi?.available ? (localWifi.ssid ?? undefined) : undefined}
          onMoveLocal={(targets) => {
            setMoveError(null)
            if (localWifi) setMoveLocal({ targets, wifi: localWifi })
          }}
          onAction={onAction}
          onLibrary={onLibrary}
          onRunGroup={onRunGroup}
          onGroupMembers={onGroupMembers}
        />
      ) : (
        loadError ? null : <p className="muted">Loading devices&hellip;</p>
      )}
      {moveLocal && reviewedWifi ? (
        <ConfirmDialog
          title={`Move to ${reviewedWifi.ssid}`}
          confirmLabel={moving ? 'Moving…' : 'Move devices'}
          busy={moving}
          error={moveError}
          onConfirm={confirmMoveLocal}
          onCancel={() => setMoveLocal(null)}
        >
          <p>
            {moveLocal.targets.type === 'devices' ? plural(moveLocal.targets.device_ids.length, 'device') : 'The devices'} will
            leave their current network and join <strong>{reviewedWifi.ssid}</strong>, the Wi-Fi this computer is on. The
            saved password is used; nothing needs typing.
          </p>
          {reviewedWifi.warning ? (
            <p className="error" role="alert">
              {reviewedWifi.warning}
            </p>
          ) : null}
          {reviewedWifi.coordinator === 'online' ? (
            <p className="muted">
              They will connect to the online coordinator at <code>{reviewedWifi.coordinator_url}</code> from then on and
              leave this local one. If they cannot report back, the job shows “Destination unconfirmed” after two
              minutes. Check the destination coordinator before retrying.
            </p>
          ) : (
            <p className="muted">
              They will reach this coordinator at <code>{reviewedWifi.coordinator_url}</code> from then on. A device that
              cannot join comes back to the hotspot and reports the failure.
            </p>
          )}
        </ConfirmDialog>
      ) : null}
    </>
  )
}
