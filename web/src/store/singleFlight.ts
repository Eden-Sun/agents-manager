export interface SingleFlightFn {
  (): Promise<void>
  resetForTest?: () => void
}

export function singleFlight(run: () => Promise<void>, onError?: (e: unknown) => void): SingleFlightFn {
  let inflight: Promise<void> | null = null
  let again = false
  let looping = false
  let generation = 0

  const fn: SingleFlightFn = () => {
    if (inflight && looping) {
      again = true
      return inflight
    }
    looping = true
    const currentGen = generation
    const p: Promise<void> = (async () => {
      try {
        do {
          again = false
          try {
            await run()
          } catch (e) {
            if (!onError) throw e
            onError(e)
          }
        } while (again && generation === currentGen)
      } finally {
        if (generation === currentGen) {
          looping = false
        }
      }
    })().finally(() => {
      if (generation === currentGen && inflight === p) {
        inflight = null
      }
    })
    inflight = p
    return p
  }

  fn.resetForTest = () => {
    generation++
    inflight = null
    again = false
    looping = false
  }

  return fn
}

export function acceptStateSeq(applied: number, incoming: number): number | null {
  if (!Number.isFinite(incoming)) return applied
  return incoming < applied ? null : incoming
}
