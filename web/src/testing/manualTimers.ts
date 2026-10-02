import { ManualClock } from '../api/mockClock'

/**
 * 把全域 `setTimeout`／`clearTimeout` 換成手動時鐘（`clock.advance(ms)` 才往前）；回傳的 `restore()` 還原。
 * 給「被測的程式自己排 timer（debounce、重試）」的測試用：不必睡到牆鐘過去，高負載下也不會因為 timer 被拖長而順序亂掉。
 */
export function installManualTimers(): { clock: ManualClock; restore: () => void } {
  const clock = new ManualClock()
  const real = { setTimeout: globalThis.setTimeout, clearTimeout: globalThis.clearTimeout }
  globalThis.setTimeout = ((fn: () => void, ms?: number) => clock.schedule(fn, ms ?? 0)) as unknown as typeof setTimeout
  globalThis.clearTimeout = ((id: unknown) => clock.cancel(id)) as unknown as typeof clearTimeout
  return { clock, restore: () => Object.assign(globalThis, real) }
}
