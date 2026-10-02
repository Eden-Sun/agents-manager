/**
 * 建 bot 的冪等鍵（daemon `POST /api/projects/:id/bots` 的 `client_request_id`，issue #352）。
 *
 * 回應遺失（連線斷在 daemon 建完之後）時，使用者會再點一次同一個動作：要拿到**同一個**鍵，daemon 才認得那是同一件事、
 * 拿回第一次建好的那顆，而不是（快速新增帶 `name_auto`）多建一顆。所以鍵要活過「失敗」：同一個動作（`key`）一直沿用同一個鍵，
 * 直到成功才作廢；放 sessionStorage，頁面重整也在。dedupe 的權威在 daemon（記在 config.toml），這裡只負責不亂換鍵。
 */
const PREFIX = 'am.createReq.'
/** 鍵是什麼時候產生的（毫秒）：只有帶 `maxAgeMs` 的呼叫端才用得到。 */
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
    /* 退回記憶體那份 */
  }
  return memAt.get(key) ?? null
}

/** 這個動作現在留著的鍵（不產新的）；沒有＝`null`。 */
export function peekRequestId(key: string): string | null {
  return read(key)
}

export interface RequestIdOptions {
  /**
   * 留著的鍵超過這麼久就不再沿用。重送（回應遺失、使用者再按一次）只在剛失敗後的一小段時間有意義；之後 daemon 若早已收下，
   * 沿用舊鍵會被當成重送、回舊結果——新的這一次根本沒送出去，畫面卻以為送了。沒帶＝一直沿用到成功（建 bot 那類靠 daemon 去重的動作）。
   * 沒有時間紀錄的舊鍵（升級前留在 sessionStorage 的）一律當過期。
   */
  maxAgeMs?: number
  now?: number
}

/** 這個動作現在要用的鍵：還沒成功過（而且沒過期）就沿用上一次的，沒有才產一個新的。 */
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
    /* 沒有 sessionStorage 也行：記憶體那份至少活過這個頁面的重試 */
  }
  return id
}

/** 成功（或 daemon 說這個鍵已經是另一件事）：作廢，下一次是新的動作。 */
export function settleCreateRequest(key: string): void {
  mem.delete(key)
  memAt.delete(key)
  try {
    globalThis.sessionStorage?.removeItem(PREFIX + key)
    globalThis.sessionStorage?.removeItem(AT_PREFIX + key)
  } catch {
    /* ignore */
  }
}
