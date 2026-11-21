export type BlobLease = { url: Promise<string>; release: () => void }

type Entry = { url: Promise<string>; users: number }

export function createBlobUrlCache(
  max: number,
  revokeUrl: (url: string) => void = (u) => URL.revokeObjectURL(u),
) {
  const cache = new Map<string, Entry>()
  const revoke = (e: Entry) => void e.url.then((u) => revokeUrl(u), () => {})

  function trim() {
    let idle = 0
    for (const e of cache.values()) if (e.users === 0) idle++
    for (const [key, e] of cache) {
      if (idle <= max) break
      if (e.users > 0) continue
      cache.delete(key)
      revoke(e)
      idle--
    }
  }

  function acquire(key: string, load: () => Promise<string>): BlobLease {
    let entry = cache.get(key)
    if (entry) {
      cache.delete(key)
    } else {
      const url = load()
      const created: Entry = { url, users: 0 }
      entry = created
      url.catch(() => {
        if (cache.get(key) === created) cache.delete(key)
      })
    }
    cache.set(key, entry)
    entry.users++
    const held = entry
    let released = false
    return {
      url: entry.url,
      release: () => {
        if (released) return
        released = true
        held.users--
        if (cache.get(key) === held) trim()
      },
    }
  }

  function clear() {
    for (const e of cache.values()) revoke(e)
    cache.clear()
  }

  return { acquire, clear, size: () => cache.size }
}
