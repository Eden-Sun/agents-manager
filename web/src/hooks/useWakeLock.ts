import { useEffect, useState } from 'react'
import { loadKeepAwake, saveKeepAwake, wakeSupport, type WakeSupport } from '../lib/wakeLock'

interface Sentinel {
  released: boolean
  release: () => Promise<void>
  addEventListener: (type: 'release', cb: () => void) => void
}

/**
 * 「螢幕保持亮著」開關（`lib/wakeLock.ts`）。鎖會在分頁切走時被系統收回，所以回到前景要自己重拿，
 * 不然使用者以為還開著、螢幕卻睡了。關掉或離開頁面就釋放。
 */
export function useWakeLock(): { on: boolean; setOn: (v: boolean) => void; support: WakeSupport; active: boolean } {
  const [on, setOnState] = useState(loadKeepAwake)
  const [active, setActive] = useState(false)
  const support = wakeSupport()

  const setOn = (v: boolean) => {
    saveKeepAwake(v)
    setOnState(v)
  }

  useEffect(() => {
    // 關著就什麼都不做；`active` 在 return 那裡跟 `on` 一起算，不在 effect 裡同步 setState。
    if (!on || support !== 'ok') return
    let alive = true
    let held: Sentinel | null = null
    const acquire = async () => {
      if (!alive || document.visibilityState !== 'visible' || held) return
      try {
        const lock = (await (navigator as unknown as { wakeLock: { request: (t: 'screen') => Promise<Sentinel> } }).wakeLock.request(
          'screen',
        )) as Sentinel
        if (!alive) {
          void lock.release()
          return
        }
        held = lock
        setActive(true)
        // 系統收回（切走分頁、鎖屏）時要知道，不然下次 `acquire` 以為還握著。
        lock.addEventListener('release', () => {
          if (held === lock) held = null
          setActive(false)
        })
      } catch {
        // 使用者拒絕、或系統省電模式不給：當成沒開，開關留在原處讓人再試。
        setActive(false)
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
      setActive(false)
      if (lock && !lock.released) void lock.release().catch(() => {})
    }
  }, [on, support])

  return { on, setOn, support, active: on && support === 'ok' && active }
}
