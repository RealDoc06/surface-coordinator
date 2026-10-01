import { useMemo } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'
import type { TargetSpec } from '../api/types'
import { DeviceList } from '../components/DeviceList'
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
  const { state } = useStore()
  const navigate = useNavigate()
  const location = useLocation()
  const now = useNow(15_000)

  const devices = useMemo(() => sortedDevices(state), [state.devices])
  const labels = useMemo(() => keyLabels(state), [state.keys])
  const online = devices.filter((d) => d.online).length

  function onRun(targets: TargetSpec) {
    const request: ComposeRequest = { targets }
    navigate('/run', { state: request })
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
          onRun={onRun}
        />
      ) : (
        <p className="muted">Loading devices&hellip;</p>
      )}
    </>
  )
}
