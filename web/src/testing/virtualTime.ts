import { act } from './domHarness'
import { installManualClockTimers } from './manualClockTimers'

export function virtualTime() {
  const timers = installManualClockTimers()
  const realNow = Date.now
  const base = realNow()
  Date.now = () => base + timers.clock.now()
  const tick = (ms: number) =>
    act(async () => {
      await timers.clock.advance(ms)
      await new Promise<void>((resolve) => setImmediate(resolve))
    })
  const waitFor = async (cond: () => boolean, what: string, maxMs = 8000, step = 50) => {
    for (let spent = 0; spent <= maxMs; spent += step) {
      if (cond()) return
      await tick(step)
    }
    throw new Error(`等不到：${what}`)
  }
  return {
    base,
    tick,
    waitFor,
    restore: () => {
      Date.now = realNow
      timers.restore()
    },
  }
}
