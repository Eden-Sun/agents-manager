export interface BusyLock {
  begin(): void
  end(): void
}

export function nestableBusy(setBusy: (busy: boolean) => void): BusyLock {
  let depth = 0
  return {
    begin() {
      depth++
      if (depth === 1) setBusy(true)
    },
    end() {
      if (depth === 0) return
      depth--
      if (depth === 0) setBusy(false)
    },
  }
}
