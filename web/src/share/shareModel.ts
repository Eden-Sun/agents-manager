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
}

/** 單則文字上限（daemon 另有自己的上限，超過回 413）；單檔 25 MiB 同契約 C。 */
export const SHARE_TEXT_MAX = 8000
export const SHARE_FILE_MAX = 25 * 1024 * 1024

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
    text: str(o.text) || str(o.content),
    created_at: str(o.created_at),
    attachments: atts.map((a) => ({ name: typeof a === 'string' ? a : str(rec(a).name) })).filter((a) => a.name),
  }
}

export function toStatus(v: unknown): ShareStatus {
  const s = typeof v === 'string' ? v : str(rec(v).status)
  return s === 'working' || s === 'thinking' ? 'working' : 'idle'
}

export function toSharePage(v: unknown): SharePage {
  const o = rec(v)
  const msgs = Array.isArray(o.messages) ? o.messages : []
  return {
    bot_name: str(o.bot_name) || str(rec(o.bot).name),
    status: toStatus(o.status),
    messages: msgs.map(toShareMessage).filter((m): m is ShareMessage => m !== null),
    has_more: o.has_more === true,
  }
}

export function toShareFiles(v: unknown): ShareFile[] {
  const list = Array.isArray(rec(v).files) ? (rec(v).files as unknown[]) : []
  return list
    .map((f) => {
      const o = rec(f)
      const name = str(o.name)
      return name ? { name, size: typeof o.size === 'number' ? o.size : 0, modified_at: str(o.modified_at) || str(o.mtime) || null } : null
    })
    .filter((f): f is ShareFile => f !== null)
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
    if (e.status === 409) return '對方正在忙，請稍後再送。'
  }
  if (what === 'upload') return '上傳失敗，請再試一次。'
  if (what === 'send') return '送出失敗，請再試一次；你打的字還在。'
  return '連不上，正在重試…'
}
