import { act } from './domHarness'
import { installManualClockTimers } from './manualClockTimers'

/**
 * 元件自己的 `setTimeout`／`setInterval` 與 `Date.now()` 一起走手動時鐘：`tick(ms)` 才往前，不等牆鐘。
 * 換上去後 `settle`／`until` 這類靠真 timer 的 helper 會卡住，所以用 `waitFor`；
 * 呼叫端要用 `try … finally { vt.restore() }` 包住，`restore()` 要早於 `unmountAll`。
 */
export function virtualTime() {
  const timers = installManualClockTimers()
  const realNow = Date.now
  const base = realNow()
  Date.now = () => base + timers.clock.now()
  /** 往前推 `ms`，每個 timer 之間讓 promise 鏈跑完；推完再讓出一輪。 */
  const tick = (ms: number) =>
    act(async () => {
      await timers.clock.advance(ms)
      await new Promise<void>((resolve) => setImmediate(resolve))
    })
  /** 每次推進 `step` ms 看一次條件，最多推進 `maxMs`（失敗訊息同 `until`）。 */
  const waitFor = async (cond: () => boolean, what: string, maxMs = 8000, step = 50) => {
    for (let spent = 0; spent <= maxMs; spent += step) {
      if (cond()) return
      await tick(step)
    }
    throw new Error(`等不到：${what}`)
  }
  return {
    /** 起點的真實 `Date.now()`（要造「N ms 之後」的時間戳用）。 */
    base,
    tick,
    waitFor,
    restore: () => {
      Date.now = realNow
      timers.restore()
    },
  }
}
