import { act } from './domHarness'
import type { ManualClock } from '../api/mockClock'

export interface UntilVirtualOptions {
  maxVirtualMs?: number
  stepMs?: number
  alsoAdvance?: (ms: number) => Promise<void>
}

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
