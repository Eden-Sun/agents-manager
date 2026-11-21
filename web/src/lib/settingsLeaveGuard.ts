type Guard = () => boolean

let current: Guard | null = null

export function registerSettingsLeaveGuard(fn: Guard): () => void {
  current = fn
  return () => {
    if (current === fn) current = null
  }
}

export function settingsBlocksLeave(): boolean {
  return current ? current() : false
}
