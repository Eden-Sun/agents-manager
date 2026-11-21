
type Timer = { at: number; id: number; fn: () => void }

export class ManualClock {
  private t = 0
  private seq = 0
  private readonly timers = new Map<number, Timer>()

  now(): number {
    return this.t
  }

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

export const manualMockClock = new ManualClock()
let manual = false

export function useManualMockClock(on: boolean): ManualClock {
  manual = on
  return manualMockClock
}

export function later(fn: () => void, ms: number): unknown {
  return manual ? manualMockClock.schedule(fn, ms) : setTimeout(fn, ms)
}

export function sleep(ms: number): Promise<void> {
  return manual ? Promise.resolve() : new Promise<void>((resolve) => setTimeout(resolve, ms))
}
