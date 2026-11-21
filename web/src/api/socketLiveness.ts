
export const SILENCE_MS = 60_000

export const RESUME_STALE_MS = 35_000

export const STABLE_MS = 10_000

export const TOKEN_REFRESH_AFTER_FAILS = 2

export const TOKEN_REFRESH_MIN_GAP_MS = 10_000

export const CHECK_MS = 10_000

export const CONNECT_TIMEOUT_MS = 8_000

export interface LivenessDeps {
  now: () => number
  every: (fn: () => void, ms: number) => () => void
}

export const realLivenessDeps: LivenessDeps = {
  now: () => Date.now(),
  every: (fn, ms) => {
    const id = setInterval(fn, ms)
    return () => clearInterval(id)
  },
}

export function isForeground(): boolean {
  return typeof document === 'undefined' || document.visibilityState !== 'hidden'
}
