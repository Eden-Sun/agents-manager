import { t } from './shareI18n'

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
  version?: string | null
}

export function shareTextChars(s: string): number {
  return [...s].length
}

export const SHARE_TEXT_MAX = 8000
export const SHARE_FILE_MAX = 25 * 1024 * 1024
export const SHARE_PAGE_MAX = 100
export const SHARE_TEXT_STORE_MAX = 100_000

const TOKEN_RE = /^[A-Za-z0-9_-]{16,128}$/

export function tokenFromLocation(loc: { pathname: string; search: string }): string | null {
  const m = loc.pathname.match(/^\/s\/([^/]+)\/?$/)
  let raw: string | null
  try {
    raw = m ? decodeURIComponent(m[1]) : new URLSearchParams(loc.search).get('token')
  } catch {
    return null
  }
  return raw && TOKEN_RE.test(raw) ? raw : null
}

const rec = (v: unknown): Record<string, unknown> => (typeof v === 'object' && v !== null ? (v as Record<string, unknown>) : {})
const str = (v: unknown): string => (typeof v === 'string' ? v : '')

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
  return s === 'working' || s === 'thinking' || s === 'blocked' || s === 'starting' ? 'working' : 'idle'
}

export function toSharePage(v: unknown): SharePage {
  const o = rec(v)
  const msgs = Array.isArray(o.messages) ? o.messages : []
  const kept = msgs.slice(-SHARE_PAGE_MAX)
  return {
    bot_name: str(o.bot_name) || str(rec(o.bot).name),
    status: toStatus(o.status),
    messages: kept.map(toShareMessage).filter((m): m is ShareMessage => m !== null),
    has_more: o.has_more === true || msgs.length > kept.length,
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

const SHARE_MARKS = ['〔分享使用者上傳的檔案，在工作目錄的 inbox/ 底下〕', '〔分享使用者〕 ', '〔分享使用者〕']

export function stripShareMarks(s: string): string {
  let out = s
  for (const m of SHARE_MARKS) out = out.split(m).join('')
  return out
}

function capShareText(s: string): string {
  if (s.length <= SHARE_TEXT_STORE_MAX) return s
  let end = SHARE_TEXT_STORE_MAX
  const c = s.charCodeAt(end - 1)
  if (c >= 0xd800 && c <= 0xdbff) end -= 1
  return `${s.slice(0, end)}\n${t('…（內容過長，已截斷）')}`
}

export function shareBeforeCursor(id: string | undefined): string | undefined {
  if (!id || !/^[A-Za-z0-9_-]{1,64}$/.test(id)) return undefined
  return id
}

function hasSpaceOrControl(s: string): boolean {
  for (let i = 0; i < s.length; i++) if (s.charCodeAt(i) <= 0x20) return true
  return false
}

export function shareSafeHref(href: string | null | undefined): string | undefined {
  if (!href || href !== href.trim() || hasSpaceOrControl(href)) return undefined
  try {
    const u = new URL(href)
    return u.protocol === 'http:' || u.protocol === 'https:' ? href : undefined
  } catch {
    return undefined
  }
}

export function mergeMessages(a: readonly ShareMessage[], b: readonly ShareMessage[]): ShareMessage[] {
  const map = new Map<string, ShareMessage>()
  for (const m of a) map.set(m.id, m)
  for (const m of b) map.set(m.id, m)
  return [...map.values()].sort((x, y) => (x.created_at === y.created_at ? x.id.localeCompare(y.id) : x.created_at.localeCompare(y.created_at)))
}

export function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`
  const kb = n / 1024
  const one = Math.round(kb * 10) / 10
  if (Math.round(kb) < 1024) return `${one < 10 ? one.toFixed(1) : Math.round(kb)} KB`
  const mb = n / 1024 / 1024
  return Math.round(mb * 10) / 10 === 10 ? '10 MB' : `${mb.toFixed(1)} MB`
}

export class ShareHttpError extends Error {
  status: number
  retryAfter: number | null
  reason: string | null
  constructor(status: number, retryAfter: number | null = null, reason: string | null = null) {
    super(`HTTP ${status}`)
    this.status = status
    this.retryAfter = retryAfter
    this.reason = reason
  }
}

export function shareErrorText(e: unknown, what: 'send' | 'upload' | 'load' | 'older'): string {
  if (e instanceof ShareHttpError) {
    if (e.status === 404) return t('這個分享連結已失效。')
    if (e.status === 429) return e.retryAfter ? t('傳得太快了，請 {sec} 秒後再試。', { sec: e.retryAfter }) : t('傳得太快了，請稍等一下再試。')
    if (e.status === 413) return what === 'upload' ? t('檔案太大了（單檔上限 25 MB）。') : t('訊息太長了，請分成幾段送。')
    if (e.status === 415) return t('不支援這種檔案。')
    if (e.status === 507) return t('空間滿了，請跟分享給你的人說一聲；你打的字還在。')
    if (e.status === 409) return t('上一則還沒回完，等 bot 回完再送；你打的字還在。')
    if (e.status === 400 && e.reason === 'unknown_attachment') return t('有附件已經不在了，請把附件移除、重新選一次再送；你打的字還在。')
  }
  if (what === 'upload') return t('上傳失敗，請再試一次。')
  if (what === 'send') return t('送出失敗，請再試一次；你打的字還在。')
  if (what === 'older') return t('連不上，請按重試再試一次。')
  return t('連不上，正在重試…')
}
