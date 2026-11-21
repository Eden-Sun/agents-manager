import { ManualClock } from '../api/mockClock'

export function installManualClockTimers(): { clock: ManualClock; restore: () => void } {
  const clock = new ManualClock()
  const real = {
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    setInterval: globalThis.setInterval,
    clearInterval: globalThis.clearInterval,
  }
  const repeating = new Map<number, number>()
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
    return -handle
  }) as unknown as typeof setInterval
  globalThis.clearInterval = ((id: unknown) => {
    if (typeof id !== 'number') return
    const clockId = repeating.get(-id)
    if (clockId !== undefined) clock.cancel(clockId)
    repeating.delete(-id)
  }) as unknown as typeof clearInterval
  return { clock, restore: () => Object.assign(globalThis, real) }
}
