import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
  type Dispatch,
  type ReactNode,
} from 'react'
import { api, ApiError, errorMessage } from '../api/client'
import { EventStream } from '../api/events'
import type { Group } from '../api/types'
import { initialState, reducer, type Action, type State } from './store'

type StoreContextValue = {
  state: State
  dispatch: Dispatch<Action>
  /** Set when the data every view depends on could not be loaded. */
  loadError: string | null
  /** Refetch everything the current views show. */
  reload: () => void
  /** Reopen the events connection immediately. */
  reconnect: () => void
  /** Refetch the enrollment keys after changing them. */
  reloadKeys: () => Promise<void>
  /** Refetch groups and devices after changing groups (the coordinator also announces it). */
  reloadGroups: () => void
}

const StoreContext = createContext<StoreContextValue | null>(null)

/** Groups arrived with milestone 4: an older coordinator answers 404, meaning there are none. */
async function listGroupsIfSupported(): Promise<Group[]> {
  try {
    return (await api.listGroups()).groups
  } catch (err) {
    if (err instanceof ApiError && err.status === 404) return []
    throw err
  }
}

type Props = {
  children: ReactNode
  /** The events socket dropped; a chance to check the session is still valid. */
  onDisconnected?: () => void
}

/**
 * Owns the application state, the single events WebSocket, and the data every
 * view needs (devices, enrollment key labels, rejected connections). Views
 * load anything else themselves, keyed on `state.syncEpoch`.
 */
export function StoreProvider({ children, onDisconnected }: Props) {
  const [state, dispatch] = useReducer(reducer, initialState)
  const [loadError, setLoadError] = useState<string | null>(null)
  const streamRef = useRef<EventStream | null>(null)

  useEffect(() => {
    let output: unknown[] = []
    let outputTimer: ReturnType<typeof setTimeout> | null = null
    const flushOutput = () => {
      if (outputTimer !== null) clearTimeout(outputTimer)
      outputTimer = null
      if (output.length > 0) dispatch({ type: 'events', events: output })
      output = []
    }
    const stream = new EventStream({
      onEvent: (event) => {
        // Noisy scripts can send hundreds of chunks a second. Render their
        // output in 32 ms batches; device and job states stay immediate.
        if (typeof event === 'object' && event !== null && 'type' in event && event.type === 'job.output') {
          output.push(event)
          if (outputTimer === null) outputTimer = setTimeout(flushOutput, 32)
          if (output.length >= 256) flushOutput()
          return
        }
        flushOutput()
        dispatch({ type: 'event', event })
      },
      onStatus: (status) => {
        flushOutput()
        dispatch({ type: 'connection', status })
        if (status === 'reconnecting') onDisconnected?.()
      },
    })
    streamRef.current = stream
    stream.start()
    return () => {
      if (outputTimer !== null) clearTimeout(outputTimer)
      streamRef.current = null
      stream.stop()
    }
    // The stream lives as long as the provider; onDisconnected is not a dependency.
  }, [])

  useEffect(() => {
    let cancelled = false
    dispatch({ type: 'base.requested' })
    Promise.all([api.listDevices(), api.listKeys(), api.listRejected(), listGroupsIfSupported()]).then(
      ([devices, keys, rejected, groups]) => {
        if (cancelled) return
        dispatch({
          type: 'base.loaded',
          devices: devices.devices,
          keys: keys.keys,
          rejected: rejected.rejected,
        })
        dispatch({ type: 'groups.loaded', groups })
        setLoadError(null)
      },
      (err: unknown) => {
        if (cancelled) return
        dispatch({ type: 'base.failed' })
        setLoadError(errorMessage(err))
      },
    )
    return () => {
      cancelled = true
    }
  }, [state.syncEpoch, state.groupsEpoch])

  // The catalogue only changes with the coordinator's version. It is optional:
  // previews fall back to setting ids when it cannot be loaded.
  useEffect(() => {
    let cancelled = false
    api.getSettingsCatalogue().then(
      ({ settings }) => {
        if (!cancelled) dispatch({ type: 'catalogue.loaded', settings })
      },
      () => {},
    )
    return () => {
      cancelled = true
    }
  }, [state.syncEpoch])

  const reload = useCallback(() => dispatch({ type: 'sync' }), [])
  const reconnect = useCallback(() => streamRef.current?.reconnect(), [])
  const reloadKeys = useCallback(async () => {
    const { keys } = await api.listKeys()
    dispatch({ type: 'keys.loaded', keys })
  }, [])

  const reloadGroups = useCallback(() => dispatch({ type: 'event', event: { type: 'groups.changed' } }), [])

  const value = useMemo(
    () => ({ state, dispatch, loadError, reload, reconnect, reloadKeys, reloadGroups }),
    [state, loadError, reload, reconnect, reloadKeys, reloadGroups],
  )
  return <StoreContext.Provider value={value}>{children}</StoreContext.Provider>
}

export function useStore(): StoreContextValue {
  const value = useContext(StoreContext)
  if (!value) throw new Error('useStore must be used inside StoreProvider')
  return value
}
