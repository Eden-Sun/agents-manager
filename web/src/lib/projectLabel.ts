export function dirLabel(path: string): string {
  return path.split('/').filter(Boolean).pop() ?? ''
}

export function labelAfterPick(current: string, lastAuto: string, picked: string): string {
  return !current.trim() || current === lastAuto ? dirLabel(picked) : current
}
