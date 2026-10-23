import { act } from './domHarness'
import type { ManualClock } from '../api/mockClock'

export interface UntilVirtualOptions {
  /** 虛擬時間的上限（不是牆鐘）。 */
  maxVirtualMs?: number
  /** 每輪推進幾虛擬毫秒。 */
  stepMs?: number
  /** 每輪另外要一起往前的時鐘（例如 `advanceMockTime`）。 */
  alsoAdvance?: (ms: number) => Promise<void>
}

/**
 * 手動時鐘版的 `until`：`installManualTimers`／`installManualClockTimers` 換上去之後 `until`／`settle` 的真 timer 會卡住，
 * 這裡每輪把時鐘往前推 `stepMs` 虛擬毫秒（元件的 `sleep`／輪詢跟著到期），再讓 React 與 promise 跑完。
 * 逾時看的是虛擬時間，不是牆鐘：高負載下也不會因為機器慢而提早放棄。
 */
export async function untilVirtual(clock: ManualClock, cond: () => boolean | Promise<boolean>, what: string, opts: UntilVirtualOptions = {}): Promise<void> {
  const { maxVirtualMs = 10_000, stepMs = 50, alsoAdvance } = opts
  for (let t = 0; t <= maxVirtualMs; t += stepMs) {
    if (await cond()) return
    await act(async () => {
      await clock.advance(stepMs)
      await alsoAdvance?.(stepMs)
    })
  }
  throw new Error(`等不到：${what}`)
}
