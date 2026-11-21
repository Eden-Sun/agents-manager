export function createHitSearch<T>(search: (q: string) => Promise<T>, apply: (hits: T | null) => void, delayMs = 250) {
  let seq = 0
  let timer: ReturnType<typeof setTimeout> | null = null
  const stop = () => {
    if (timer) clearTimeout(timer)
    timer = null
  }
  return {
    run(query: string) {
      stop()
      const q = query.trim()
      const mine = ++seq
      if (!q) {
        apply(null)
        return
      }
      timer = setTimeout(() => {
        void search(q)
          .then((r) => {
            if (seq === mine) apply(r)
          })
          .catch(() => {
            if (seq === mine) apply(null)
          })
      }, delayMs)
    },
    cancel() {
      seq += 1
      stop()
    },
  }
}
