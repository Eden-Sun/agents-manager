/**
 * 分享頁（`web/share.html`，SPEC「分享 bot」）的純邏輯：token 從網址取、契約 C 的回應正規化、訊息合併、錯誤轉人話。
 * 這一包不 import 主 UI 的任何東西（store、api、元件）：分享頁打包出來不能帶著管理介面的程式碼。
 */

export type ShareRole = 'user' | 'assistant'
export type ShareStatus = 'idle' | 'working'

export interface ShareMessage {
  id: string
  role: ShareRole
  text: string
  created_at: string
  attachments: { name: string }[]
}

export interface SharePage {
  bot_name: string
  status: ShareStatus
  messages: ShareMessage[]
  has_more: boolean
}

export interface ShareFile {
  name: string
  size: number
  modified_at: string | null
  /** daemon 給的不透明檔案版本（inode／大小／奈秒時間）；同秒同大小重寫也會變。舊 daemon 沒有。 */
  version?: string | null
}

/** 單則文字上限（daemon 另有自己的上限，超過回 413）；單檔 25 MiB 同契約 C。 */
export const SHARE_TEXT_MAX = 8000
export const SHARE_FILE_MAX = 25 * 1024 * 1024
/** 一頁最多畫這麼多則；多的丟掉，避免一份超大 JSON 把分頁整個掛上。 */
export const SHARE_PAGE_MAX = 100
/** 單則進畫面的字數上限。再長就截斷，避免一則幾 MB 的 `<pre>`。 */
export const SHARE_TEXT_STORE_MAX = 100_000

/** token 只認 base64url（契約 B：≥32 bytes 隨機）。 */
const TOKEN_RE = /^[A-Za-z0-9_-]{16,128}$/

/** 正式網址是 `/s/<token>`；dev／mock 用 `share.html?token=<token>`。對不上回 null（畫「連結無效」）。 */
export function tokenFromLocation(loc: { pathname: string; search: string }): string | null {
  const m = loc.pathname.match(/^\/s\/([^/]+)\/?$/)
  const raw = m ? decodeURIComponent(m[1]) : new URLSearchParams(loc.search).get('token')
  return raw && TOKEN_RE.test(raw) ? raw : null
}

const rec = (v: unknown): Record<string, unknown> => (typeof v === 'object' && v !== null ? (v as Record<string, unknown>) : {})
const str = (v: unknown): string => (typeof v === 'string' ? v : '')

/** 契約 C 只給 role、text、時間、附件名；其他角色（system 之類）不畫。 */
export function toShareMessage(v: unknown): ShareMessage | null {
  const o = rec(v)
  const role = o.role
  const id = str(o.id)
  if (!id || (role !== 'user' && role !== 'assistant')) return null
  const atts = Array.isArray(o.attachments) ? o.attachments : []
  return {
    id,
    role,
    text: capShareText(str(o.text) || str(o.content)),
    created_at: str(o.created_at),
    attachments: atts.map((a) => ({ name: typeof a === 'string' ? a : str(rec(a).name) })).filter((a) => a.name),
  }
}

export function toStatus(v: unknown): ShareStatus {
  const s = typeof v === 'string' ? v : str(rec(v).status)
  // daemon 的 status 是 lamp（API.md §5.6）：blocked（等擁有者在終端處理）與 starting 對 end user 一樣是「還沒回完、先別送」。
  return s === 'working' || s === 'thinking' || s === 'blocked' || s === 'starting' ? 'working' : 'idle'
}

export function toSharePage(v: unknown): SharePage {
  const o = rec(v)
  const msgs = Array.isArray(o.messages) ? o.messages : []
  return {
    bot_name: str(o.bot_name) || str(rec(o.bot).name),
    status: toStatus(o.status),
    messages: msgs.slice(0, SHARE_PAGE_MAX).map(toShareMessage).filter((m): m is ShareMessage => m !== null),
    has_more: o.has_more === true,
  }
}

export function toShareFiles(v: unknown): ShareFile[] {
  const list = Array.isArray(rec(v).files) ? (rec(v).files as unknown[]) : []
  return list
    .map((f): ShareFile | null => {
      const o = rec(f)
      const name = str(o.name)
      return name ? { name, size: typeof o.size === 'number' ? o.size : 0, modified_at: str(o.modified_at) || str(o.mtime) || null, version: str(o.version) || null } : null
    })
    .filter((f): f is ShareFile => f !== null)
}

/** daemon 替 end user 訊息加的前綴與附件標記（daemon 輸出時已拿掉；這裡再擋一次，舊版 daemon 或 bot 照抄時也不會出現）。 */
const SHARE_MARKS = ['〔分享使用者上傳的檔案，在工作目錄的 inbox/ 底下〕', '〔分享使用者〕 ', '〔分享使用者〕']

export function stripShareMarks(s: string): string {
  let out = s
  for (const m of SHARE_MARKS) out = out.split(m).join('')
  return out
}

function capShareText(s: string): string {
  if (s.length <= SHARE_TEXT_STORE_MAX) return s
  return `${s.slice(0, SHARE_TEXT_STORE_MAX)}\n…（內容過長，已截斷）`
}

/**
 * 分享頁連結只留絕對的 http／https。`javascript:`、`data:`、`//host` 都不給，
 * 避免 bot 回覆在這個 origin 上執行，或把帶 token 的網址送去別的站。
 */
/** 分頁游標只收單一 id。空白、斜線、另一段 query 都不送，避免 before 被拿去指到別的路徑。 */
export function shareBeforeCursor(id: string | undefined): string | undefined {
  if (!id || !/^[A-Za-z0-9_-]{1,64}$/.test(id)) return undefined
  return id
}

export function shareSafeHref(href: string | null | undefined): string | undefined {
  if (!href || href !== href.trim() || /[\u0000-\u0020]/.test(href)) return undefined
  try {
    const u = new URL(href)
    return u.protocol === 'http:' || u.protocol === 'https:' ? href : undefined
  } catch {
    return undefined
  }
}

/** 依 id 去重、照時間排（同時間照 id）；新的覆蓋舊的同 id。 */
export function mergeMessages(a: readonly ShareMessage[], b: readonly ShareMessage[]): ShareMessage[] {
  const map = new Map<string, ShareMessage>()
  for (const m of a) map.set(m.id, m)
  for (const m of b) map.set(m.id, m)
  return [...map.values()].sort((x, y) => (x.created_at === y.created_at ? x.id.localeCompare(y.id) : x.created_at.localeCompare(y.created_at)))
}

export function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10 * 1024 ? 1 : 0)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

export class ShareHttpError extends Error {
  status: number
  retryAfter: number | null
  constructor(status: number, retryAfter: number | null = null) {
    super(`HTTP ${status}`)
    this.status = status
    this.retryAfter = retryAfter
  }
}

/** 給 end user 看的錯誤；不帶任何內部細節。404＝連結失效（token 錯或分享已關，契約 C 不分）。 */
export function shareErrorText(e: unknown, what: 'send' | 'upload' | 'load'): string {
  if (e instanceof ShareHttpError) {
    if (e.status === 404) return '這個分享連結已失效。'
    if (e.status === 429) return e.retryAfter ? `傳得太快了，請 ${e.retryAfter} 秒後再試。` : '傳得太快了，請稍等一下再試。'
    if (e.status === 413) return what === 'upload' ? '檔案太大了（單檔上限 25 MB）。' : '訊息太長了，請分成幾段送。'
    if (e.status === 415) return '不支援這種檔案。'
    // 507 share_storage_full（#853）：沙箱滿了，送訊息被擋；打的字還在。
    if (e.status === 507) return '空間滿了，請跟分享給你的人說一聲；你打的字還在。'
    // 一段對話同時只排一則（409 not_accepted）：上一則還沒回完。
    if (e.status === 409) return '上一則還沒回完，等 bot 回完再送；你打的字還在。'
  }
  if (what === 'upload') return '上傳失敗，請再試一次。'
  if (what === 'send') return '送出失敗，請再試一次；你打的字還在。'
  return '連不上，正在重試…'
}
