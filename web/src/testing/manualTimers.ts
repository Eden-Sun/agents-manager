import { ManualClock } from '../api/mockClock'

export function installManualTimers(): { clock: ManualClock; restore: () => void } {
  const clock = new ManualClock()
  const real = { setTimeout: globalThis.setTimeout, clearTimeout: globalThis.clearTimeout }
  globalThis.setTimeout = ((fn: () => void, ms?: number) => clock.schedule(fn, ms ?? 0)) as unknown as typeof setTimeout
  globalThis.clearTimeout = ((id: unknown) => clock.cancel(id)) as unknown as typeof clearTimeout
  return { clock, restore: () => Object.assign(globalThis, real) }
}
