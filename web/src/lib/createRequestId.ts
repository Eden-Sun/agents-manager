const PREFIX = 'am.createReq.'
const AT_PREFIX = 'am.createReqAt.'
const mem = new Map<string, string>()
const memAt = new Map<string, number>()

function fresh(): string {
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto
  return c?.randomUUID ? c.randomUUID() : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 12)}`
}

function read(key: string): string | null {
  try {
    return globalThis.sessionStorage?.getItem(PREFIX + key) ?? mem.get(key) ?? null
  } catch {
    return mem.get(key) ?? null
  }
}

function readAt(key: string): number | null {
  try {
    const raw = globalThis.sessionStorage?.getItem(AT_PREFIX + key)
    const n = raw == null ? NaN : Number(raw)
    if (Number.isFinite(n)) return n
  } catch {
  }
  return memAt.get(key) ?? null
}

export function peekRequestId(key: string): string | null {
  return read(key)
}

export interface RequestIdOptions {
  maxAgeMs?: number
  now?: number
}

export function createRequestId(key: string, opts: RequestIdOptions = {}): string {
  const now = opts.now ?? Date.now()
  const existing = read(key)
  if (existing) {
    const at = opts.maxAgeMs === undefined ? null : readAt(key)
    if (opts.maxAgeMs === undefined || (at !== null && now - at <= opts.maxAgeMs)) return existing
  }
  const id = fresh()
  mem.set(key, id)
  memAt.set(key, now)
  try {
    globalThis.sessionStorage?.setItem(PREFIX + key, id)
    globalThis.sessionStorage?.setItem(AT_PREFIX + key, String(now))
  } catch {
  }
  return id
}

export function settleCreateRequest(key: string): void {
  mem.delete(key)
  memAt.delete(key)
  try {
    globalThis.sessionStorage?.removeItem(PREFIX + key)
    globalThis.sessionStorage?.removeItem(AT_PREFIX + key)
  } catch {
  }
}
