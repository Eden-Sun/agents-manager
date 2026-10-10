import { useEffect, useSyncExternalStore } from 'react'
import { loadKeepAwake, saveKeepAwake, wakeSupport, type WakeSupport } from '../lib/wakeLock'

interface Sentinel {
  released: boolean
  release: () => Promise<void>
  addEventListener: (type: 'release', cb: () => void) => void
}

/**
 * 「螢幕保持亮著」（`lib/wakeLock.ts`）。偏好與「現在有沒有握住」放在模組層，真正去要／還鎖的只有 App 根部的
 * `useWakeLockHolder`；環境設定的開關（`useWakeLock`）只讀寫偏好。所以關掉設定視窗不會放掉鎖（#1199）。
 * 鎖會在分頁切走時被系統收回，所以回到前景要自己重拿，不然使用者以為還開著、螢幕卻睡了。
 */
let pref = loadKeepAwake()
let holding = false
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

/** 開關：存偏好，並叫醒握鎖的那一處。 */
export function setKeepAwake(v: boolean): void {
  saveKeepAwake(v)
  pref = v
  emit()
}

/** 掛在 App 根部（整個 app 活著就在）：依偏好拿鎖、關掉就還。 */
export function useWakeLockHolder(): void {
  const on = useSyncExternalStore(subscribe, getPref, getPref)
  const support = wakeSupport()
  useEffect(() => {
    // 關著就什麼都不做。
    if (!on || support !== 'ok') return
    let alive = true
    let held: Sentinel | null = null
    /** 同時只准一個 request 在路上：兩次 `visibilitychange` 夾在同一次 request 裡的話，
     *  第二次會再要一顆，先拿到的那顆從此沒人 release——開關關掉了螢幕還是不睡（#548 順帶，稽核時發現）。 */
    let inflight: Promise<Sentinel> | null = null
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
        setHolding(true)
        // 系統收回（切走分頁、鎖屏）時要知道，不然下次 `acquire` 以為還握著。
        lock.addEventListener('release', () => {
          if (held === lock) held = null
          setHolding(false)
        })
      } catch {
        // 使用者拒絕、或系統省電模式不給：當成沒開，開關留在原處讓人再試。
        setHolding(false)
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
      document.removeEventListener('visibilitychange', onVisible)
      const lock = held
      held = null
      inflight = null
      setHolding(false)
      if (lock && !lock.released) void lock.release().catch(() => {})
      // 還在路上的那一顆由 `acquire` 自己收（它 await 完會看到 `alive === false` 就 release）。
    }
  }, [on, support])
}

/** 開關元件用：讀寫偏好、顯示現在有沒有握住；不自己拿鎖。 */
export function useWakeLock(): { on: boolean; setOn: (v: boolean) => void; support: WakeSupport; active: boolean } {
  const on = useSyncExternalStore(subscribe, getPref, getPref)
  const active = useSyncExternalStore(subscribe, getHolding, getHolding)
  const support = wakeSupport()
  return { on, setOn: setKeepAwake, support, active: on && support === 'ok' && active }
}
