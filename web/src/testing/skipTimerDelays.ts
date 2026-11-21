export function skipTimerDelays(delays: readonly number[]): { restore: () => void } {
  const real = { setTimeout: globalThis.setTimeout, clearTimeout: globalThis.clearTimeout }
  const skipped = new Map<object, ReturnType<typeof setImmediate>>()
  globalThis.setTimeout = ((handler: TimerHandler, timeout?: number, ...args: unknown[]) => {
    if (typeof handler === 'function' && timeout !== undefined && delays.includes(timeout)) {
      const handle = {}
      skipped.set(handle, setImmediate(() => {
        skipped.delete(handle)
        handler(...args)
      }))
      return handle as unknown as ReturnType<typeof setTimeout>
    }
    return real.setTimeout(handler, timeout, ...args)
  }) as typeof setTimeout
  globalThis.clearTimeout = ((handle?: ReturnType<typeof setTimeout>) => {
    const immediate = skipped.get(handle as unknown as object)
    if (immediate !== undefined) {
      clearImmediate(immediate)
      skipped.delete(handle as unknown as object)
      return
    }
    real.clearTimeout(handle)
  }) as typeof clearTimeout
  return {
    restore: () => {
      globalThis.setTimeout = real.setTimeout
      globalThis.clearTimeout = real.clearTimeout
    },
  }
}
