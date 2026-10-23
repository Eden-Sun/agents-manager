/**
 * 把全域 `setTimeout` 裡「指定長度」的等待縮成下一個 macrotask（`setImmediate`），其餘 timer 照真的走。
 * 給「被測元件裡寫死的固定等待」用（例如送鍵後等 260ms 讓畫面反應）：等的是 mock 立刻就會換好的畫面，
 * 不是真的時間，測試不必睡過去；順序與次數不變，只是不再等牆鐘。
 * 只動 `delays` 列出的長度，`settle()`／`until()` 與 React 的 timer 不受影響。回傳的 `restore()` 還原。
 */
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
