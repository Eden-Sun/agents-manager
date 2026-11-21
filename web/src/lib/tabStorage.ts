export interface KeyValueStore {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
  removeItem(key: string): void
}

const CLEARED = '\u0000cleared'

export function readTabValue<T>(
  key: string,
  parse: (raw: string) => T | null,
  stores: readonly (KeyValueStore | null | undefined)[],
): T | null {
  for (const s of stores) {
    try {
      const raw = s?.getItem(key)
      if (raw === CLEARED) return null
      if (!raw) continue
      const v = parse(raw)
      if (v !== null) return v
    } catch {
    }
  }
  return null
}

export function writeTabValue(key: string, value: string | null, stores: readonly (KeyValueStore | null | undefined)[]): void {
  stores.forEach((s, i) => {
    try {
      if (value !== null) s?.setItem(key, value)
      else if (i === 0) s?.setItem(key, CLEARED)
      else s?.removeItem(key)
    } catch {
    }
  })
}

export function browserTabStores(): [KeyValueStore | null, KeyValueStore | null] {
  const pick = (name: 'sessionStorage' | 'localStorage'): KeyValueStore | null => {
    try {
      return ((globalThis as Record<string, unknown>)[name] as KeyValueStore | undefined) ?? null
    } catch {
      return null
    }
  }
  return [pick('sessionStorage'), pick('localStorage')]
}
