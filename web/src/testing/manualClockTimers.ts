import { ManualClock } from '../api/mockClock'

/**
 * `installManualTimers` 的加強版：`setInterval`／`clearInterval` 也換成手動時鐘（`clock.advance(ms)` 才往前）。
 * 給輪詢型 hook 的測試用（`setTimeout` 鏈與 `setInterval` 都有）。換上去之後 `settle()`、`until()` 這類靠真 timer 的 helper 會卡住，
 * 所以每個測試用 `try … finally { restore() }` 包住，卸載（`unmountAll`）前一定要先還原。
 */
export function installManualClockTimers(): { clock: ManualClock; restore: () => void } {
  const clock = new ManualClock()
  const real = {
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    setInterval: globalThis.setInterval,
    clearInterval: globalThis.clearInterval,
  }
  const repeating = new Map<number, number>() // 對外的 interval id → 目前排著的 clock id
  let seq = 0
  globalThis.setTimeout = ((fn: () => void, ms?: number) => clock.schedule(fn, ms ?? 0)) as unknown as typeof setTimeout
  globalThis.clearTimeout = ((id: unknown) => clock.cancel(id)) as unknown as typeof clearTimeout
  globalThis.setInterval = ((fn: () => void, ms?: number) => {
    const handle = (seq += 1)
    const every = Math.max(1, ms ?? 0)
    const arm = () => {
      repeating.set(handle, clock.schedule(() => {
        arm()
        fn()
      }, every))
    }
    arm()
    return -handle // 負數：跟 setTimeout 的 id 不會撞
  }) as unknown as typeof setInterval
  globalThis.clearInterval = ((id: unknown) => {
    if (typeof id !== 'number') return
    const clockId = repeating.get(-id)
    if (clockId !== undefined) clock.cancel(clockId)
    repeating.delete(-id)
  }) as unknown as typeof clearInterval
  return { clock, restore: () => Object.assign(globalThis, real) }
}
