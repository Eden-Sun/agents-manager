/**
 * mock 的時間來源。mock 用 `setTimeout` 演「daemon 慢慢做完」（回合 2.4 秒、slow 回合 8 秒、啟動 2.5 秒…），平常就是真的牆鐘。
 * 測試要的是**確定性**：整樹在高負載下跑，牆鐘上「8 秒的忙碌」可能在測試還沒走到斷言時就結束了（#733 的排隊測試就是這樣紅的）。
 * 所以測試可以把它換成手動時鐘：mock 的 timer 只在測試呼叫 `advance()` 時才往前，延遲（`sleep`）直接略過。
 */

type Timer = { at: number; id: number; fn: () => void }

/** 虛擬時間：`advance(ms)` 依「到期時間、排入順序」執行 timer，每個之間讓出一輪 macrotask，讓 promise 鏈跑完。 */
export class ManualClock {
  private t = 0
  private seq = 0
  private readonly timers = new Map<number, Timer>()

  now(): number {
    return this.t
  }

  /** 還在等的 timer 數。 */
  get pending(): number {
    return this.timers.size
  }

  schedule(fn: () => void, ms: number): number {
    const id = (this.seq += 1)
    this.timers.set(id, { at: this.t + Math.max(0, ms), id, fn })
    return id
  }

  cancel(id: unknown): void {
    if (typeof id === 'number') this.timers.delete(id)
  }

  async advance(ms: number): Promise<void> {
    const target = this.t + ms
    for (;;) {
      let next: Timer | null = null
      for (const timer of this.timers.values()) {
        if (timer.at > target) continue
        if (!next || timer.at < next.at || (timer.at === next.at && timer.id < next.id)) next = timer
      }
      if (!next) break
      this.timers.delete(next.id)
      this.t = Math.max(this.t, next.at)
      next.fn()
      await new Promise<void>((resolve) => setImmediate(resolve))
    }
    this.t = target
  }
}

/** 全行程共用一個：測試檔之間 mock 留下的 timer 要接著走，不能每個檔各一份。 */
export const manualMockClock = new ManualClock()
let manual = false

/** 測試：開／關手動時鐘（關掉就回真的牆鐘；沒走完的 timer 留在時鐘裡，下次開起來接著走）。 */
export function useManualMockClock(on: boolean): ManualClock {
  manual = on
  return manualMockClock
}

/** mock 的 `setTimeout`。 */
export function later(fn: () => void, ms: number): unknown {
  return manual ? manualMockClock.schedule(fn, ms) : setTimeout(fn, ms)
}

/** mock 的「請求延遲」：手動時鐘下直接略過（延遲不是狀態，不必由測試推進）。 */
export function sleep(ms: number): Promise<void> {
  return manual ? Promise.resolve() : new Promise<void>((resolve) => setTimeout(resolve, ms))
}
