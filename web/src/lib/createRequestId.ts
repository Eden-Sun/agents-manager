/**
 * 建 bot 的冪等鍵（daemon `POST /api/projects/:id/bots` 的 `client_request_id`，issue #352）。
 *
 * 回應遺失（連線斷在 daemon 建完之後）時，使用者會再點一次同一個動作：要拿到**同一個**鍵，daemon 才認得那是同一件事、
 * 拿回第一次建好的那顆，而不是（快速新增帶 `name_auto`）多建一顆。所以鍵要活過「失敗」：同一個動作（`key`）一直沿用同一個鍵，
 * 直到成功才作廢；放 sessionStorage，頁面重整也在。dedupe 的權威在 daemon（記在 config.toml），這裡只負責不亂換鍵。
 */
const PREFIX = 'am.createReq.'
const mem = new Map<string, string>()

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

/** 這個動作現在要用的鍵：還沒成功過就沿用上一次的，沒有才產一個新的。 */
export function createRequestId(key: string): string {
  const existing = read(key)
  if (existing) return existing
  const id = fresh()
  mem.set(key, id)
  try {
    globalThis.sessionStorage?.setItem(PREFIX + key, id)
  } catch {
    /* 沒有 sessionStorage 也行：記憶體那份至少活過這個頁面的重試 */
  }
  return id
}

/** 成功（或 daemon 說這個鍵已經是另一件事）：作廢，下一次是新的動作。 */
export function settleCreateRequest(key: string): void {
  mem.delete(key)
  try {
    globalThis.sessionStorage?.removeItem(PREFIX + key)
  } catch {
    /* ignore */
  }
}
