const held = new Set<string>()

const keyOf = (host: string, paneId: string) => `${host || 'local'}/${paneId}`

export function holdLoginPane(host: string, paneId: string): () => void {
  const k = keyOf(host, paneId)
  held.add(k)
  return () => {
    held.delete(k)
  }
}

export function isHeldLoginPane(host: string, paneId: string): boolean {
  return held.has(keyOf(host, paneId))
}
