import { useEffect, useSyncExternalStore } from 'react'
import { loadKeepAwake, saveKeepAwake, wakeSupport, type WakeSupport } from '../lib/wakeLock'

interface Sentinel {
  released: boolean
  release: () => Promise<void>
  addEventListener: (type: 'release', cb: () => void) => void
}

let pref = loadKeepAwake()
let holding = false
let lastError: string | null = null
const subs = new Set<() => void>()
const emit = () => {
  for (const f of subs) f()
}
const subscribe = (f: () => void) => {
  subs.add(f)
  return () => {
    subs.delete(f)
  }
}
const setHolding = (v: boolean) => {
  if (holding === v) return
  holding = v
  emit()
}
const getPref = () => pref
const getHolding = () => holding
export const getWakeLockError = () => lastError

const recordError = (err: unknown) => {
  const msg = err instanceof Error ? (err.name || err.message) : String(err)
  if (lastError !== msg) {
    lastError = msg
    emit()
  }
}

const clearError = () => {
  if (lastError !== null) {
    lastError = null
    emit()
  }
}

export function setKeepAwake(v: boolean): void {
  saveKeepAwake(v)
  pref = v
  if (!v) {
    clearError()
  }
  emit()
}

const GESTURE_EVENTS = ['pointerdown', 'touchend', 'keydown'] as const
const GESTURE_OPTS = { capture: true, passive: true } as const

export function useWakeLockHolder(): void {
  const on = useSyncExternalStore(subscribe, getPref, getPref)
  const support = wakeSupport()
  useEffect(() => {
    if (!on || support !== 'ok') return
    let alive = true
    let held: Sentinel | null = null
    let inflight: Promise<Sentinel> | null = null
    let listeningGestures = false

    const onGesture = () => {
      void acquire()
    }

    const addGestureListeners = () => {
      if (listeningGestures || !alive) return
      listeningGestures = true
      for (const ev of GESTURE_EVENTS) {
        window.addEventListener(ev, onGesture, GESTURE_OPTS)
      }
    }

    const removeGestureListeners = () => {
      if (!listeningGestures) return
      listeningGestures = false
      for (const ev of GESTURE_EVENTS) {
        window.removeEventListener(ev, onGesture, GESTURE_OPTS)
      }
    }

    const acquire = async () => {
      if (!alive || document.visibilityState !== 'visible' || held || inflight) return
      try {
        const req = (navigator as unknown as { wakeLock: { request: (t: 'screen') => Promise<Sentinel> } }).wakeLock.request('screen')
        inflight = req
        const lock = (await req) as Sentinel
        if (!alive) {
          void lock.release()
          return
        }
        held = lock
        clearError()
        setHolding(true)
        removeGestureListeners()
        lock.addEventListener('release', () => {
          if (held !== lock) return
          held = null
          setHolding(false)
          if (alive) {
            addGestureListeners()
          }
        })
      } catch (err) {
        recordError(err)
        setHolding(false)
        if (alive && document.visibilityState === 'visible') {
          addGestureListeners()
        }
      } finally {
        inflight = null
      }
    }
    const onVisible = () => {
      if (document.visibilityState === 'visible') void acquire()
    }
    document.addEventListener('visibilitychange', onVisible)
    void acquire()
    return () => {
      alive = false
      removeGestureListeners()
      document.removeEventListener('visibilitychange', onVisible)
      const lock = held
      held = null
      inflight = null
      setHolding(false)
      clearError()
      if (lock && !lock.released) void lock.release().catch(() => {})
    }
  }, [on, support])
}

export function useWakeLock(): {
  on: boolean
  setOn: (v: boolean) => void
  support: WakeSupport
  active: boolean
  lastError: string | null
} {
  const on = useSyncExternalStore(subscribe, getPref, getPref)
  const active = useSyncExternalStore(subscribe, getHolding, getHolding)
  const lastErr = useSyncExternalStore(subscribe, getWakeLockError, getWakeLockError)
  const support = wakeSupport()
  return {
    on,
    setOn: setKeepAwake,
    support,
    active: on && support === 'ok' && active,
    lastError: on && support === 'ok' && !active ? lastErr : null,
  }
}
