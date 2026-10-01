import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useReducer,
  useState,
  type Dispatch,
  type ReactNode,
} from 'react'
import { api, errorMessage } from '../api/client'
import { EventStream } from '../api/events'
import { initialState, reducer, type Action, type State } from './store'

type StoreContextValue = {
  state: State
  dispatch: Dispatch<Action>
  /** Set when the data every view depends on could not be loaded. */
  loadError: string | null
  /** Refetch everything the current views show. */
  reload: () => void
  /** Refetch the enrollment keys after changing them. */
  reloadKeys: () => Promise<void>
}

const StoreContext = createContext<StoreContextValue | null>(null)

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

  useEffect(() => {
    const stream = new EventStream({
      onEvent: (event) => dispatch({ type: 'event', event }),
      onStatus: (status) => {
        dispatch({ type: 'connection', status })
        if (status === 'reconnecting') onDisconnected?.()
      },
    })
    stream.start()
    return () => stream.stop()
    // The stream lives as long as the provider; onDisconnected is not a dependency.
  }, [])

  useEffect(() => {
    let cancelled = false
    dispatch({ type: 'base.requested' })
    Promise.all([api.listDevices(), api.listKeys(), api.listRejected()]).then(
      ([devices, keys, rejected]) => {
        if (cancelled) return
        dispatch({
          type: 'base.loaded',
          devices: devices.devices,
          keys: keys.keys,
          rejected: rejected.rejected,
        })
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
  }, [state.syncEpoch])

  const reload = useCallback(() => dispatch({ type: 'sync' }), [])
  const reloadKeys = useCallback(async () => {
    const { keys } = await api.listKeys()
    dispatch({ type: 'keys.loaded', keys })
  }, [])

  const value = useMemo(
    () => ({ state, dispatch, loadError, reload, reloadKeys }),
    [state, loadError, reload, reloadKeys],
  )
  return <StoreContext.Provider value={value}>{children}</StoreContext.Provider>
}

export function useStore(): StoreContextValue {
  const value = useContext(StoreContext)
  if (!value) throw new Error('useStore must be used inside StoreProvider')
  return value
}
