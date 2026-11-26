import type { OutboxFile } from '../api'
import { ApiError } from '../api/types'
import type { Turn } from '../api/types'
import { t } from './i18n'

export function fileSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const num = (v: number) => {
    const one = Math.round(v * 10) / 10
    return one < 10 ? one.toFixed(1) : String(Math.round(v))
  }
  const kb = bytes / 1024
  if (kb < 1) return `${Math.round(bytes)} B`
  if (Math.round(kb) < 1024) return `${num(kb)} KB`
  const mb = bytes / 1024 ** 2
  if (Math.round(mb) < 1024) return `${num(mb)} MB`
  return `${num(bytes / 1024 ** 3)} GB`
}

export function remainingNow(file: OutboxFile, fetchedAt: number, now = Date.now()): number | null {
  if (file.remainingSecs === null) return null
  const elapsed = Math.max(0, Math.floor((now - fetchedAt) / 1000))
  return Math.max(0, file.remainingSecs - elapsed)
}

export function remainingLabel(secs: number): string {
  if (!Number.isFinite(secs) || secs <= 0) return t('即將清除')
  const mins = Math.ceil(secs / 60)
  if (mins <= 60) return t('剩 {n} 分鐘', { n: mins })
  const hours = Math.ceil(secs / 3600)
  if (hours < 48) return t('剩 {n} 小時', { n: hours })
  return t('剩 {n} 天', { n: Math.ceil(secs / 86400) })
}

export function downloadFailure(name: string, e: unknown, kept = false): { text: string; gone: boolean } {
  const plain = (text: string) => ({ text, gone: false })
  if (!(e instanceof ApiError)) return plain(t('下載「{name}」失敗：{reason}', { name, reason: e instanceof Error ? e.message : String(e) }))
  if (e.status === 404) {
    return {
      text: kept
        ? t('「{name}」已經不在了：分享用 bot 的檔案保留 14 天，總量太大時會從最舊的清掉。', { name })
        : t('「{name}」已經不在了：outbox 的檔案放進去一小時後會自動清掉。', { name }),
      gone: true,
    }
  }
  const reason = String(e.body.reason ?? e.body.error ?? '')
  if (reason === 'file_too_large') {
    const big = typeof e.body.size === 'number' ? t('（{size}）', { size: fileSize(e.body.size) }) : ''
    const max = typeof e.body.max === 'number' ? fileSize(e.body.max) : t('下載上限')
    return plain(t('「{name}」太大{size}，超過 {max}，這裡下載不了——請 bot 換個小一點的，或到那台機器上拿。', { name, size: big, max }))
  }
  if (reason === 'outbox_remote') return plain(t('「{name}」在遠端主機上，這版 daemon 還下載不了；更新 daemon 後就可以。', { name }))
  if (reason === 'outbox_remote_unreachable') return plain(t('連不上「{name}」所在的遠端主機，稍後再試。', { name }))
  return plain(t('下載「{name}」失敗：{reason}（HTTP {status}）', { name, reason: e.message, status: e.status }))
}

export const LOAD_FAILED = 'load_failed'

export type OutboxList = { dir: string; reason: string | null; ttlSecs: number; kept?: boolean; files: OutboxFile[] }

export async function readOutbox(fetchList: () => Promise<OutboxList>): Promise<OutboxList> {
  try {
    const out = await fetchList()
    return { ...out, files: orderFiles(out.files) }
  } catch {
    return { dir: '', reason: LOAD_FAILED, ttlSecs: 0, files: [] }
  }
}

export function emptyReason(reason: string | null, botSelected: boolean, kept = false): string {
  if (!botSelected) return t('先選一顆 bot，這裡會列出它交給你的檔案。')
  if (reason === LOAD_FAILED) return t('讀不到這顆 bot 的檔案清單（daemon 沒有回應或出錯），不代表沒有檔案。按上面的 ↻ 重新讀取。')
  if (reason === 'outbox_remote') return t('這顆 bot 在遠端主機上，這版 daemon 還列不出它的檔案；更新 daemon 後就看得到。')
  if (reason === 'outbox_remote_unreachable') return t('連不上這顆 bot 所在的遠端主機，暫時列不出它的檔案。按上面的 ↻ 重試。')
  if (reason === 'outbox_untrusted') return t('這顆 bot 的 outbox 不是一般資料夾（符號連結或擁有者不對），為了安全不列出、也不給下載。')
  if (kept) return t('還沒有檔案。這是分享用 bot：它放進 $AM_OUTBOX 的檔案會保留 14 天（總量太大時從最舊的清掉），外部使用者隔天回來也拿得到。')
  return t('還沒有檔案。bot 把要給你的檔案放進 $AM_OUTBOX 之後按重整就看得到；放進去 1 小時後會自動清掉。')
}

export function splitOutboxName(name: string): { dir: string; base: string } {
  const i = name.lastIndexOf('/')
  return i < 0 ? { dir: '', base: name } : { dir: name.slice(0, i + 1), base: name.slice(i + 1) }
}

export function splitOutboxExt(base: string): { stem: string; ext: string | null } {
  const dot = base.lastIndexOf('.')
  if (dot <= 0) return { stem: base, ext: null }
  const ext = base.slice(dot + 1)
  if (!/^[a-z0-9]{1,8}$/i.test(ext)) return { stem: base, ext: null }
  return { stem: base.slice(0, dot), ext: ext.toLowerCase() }
}

export type OutboxExtTone = 'pdf' | 'image' | 'doc' | 'other'

export function outboxExtTone(ext: string): OutboxExtTone {
  const e = ext.toLowerCase()
  if (e === 'pdf') return 'pdf'
  if (['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif'].includes(e)) return 'image'
  if (['ppt', 'pptx', 'key', 'doc', 'docx', 'xls', 'xlsx', 'csv', 'tsv', 'md'].includes(e)) return 'doc'
  return 'other'
}

const OUTBOX_REF = /outbox\/([0-9A-HJKMNP-TV-Z]{26})\/([^\s`'"<>()[\]|*]+)/g

export function relayedOutboxRefs(messages: ReadonlyArray<{ content: string }> | undefined, selfId: string): Record<string, string[]> {
  const out: Record<string, Set<string>> = {}
  for (const m of messages ?? []) {
    for (const hit of (m.content ?? '').matchAll(OUTBOX_REF)) {
      const [, bot, raw] = hit
      if (bot === selfId) continue
      let name = raw.replace(/[.,;:!?，。；：！？、]+$/u, '')
      try {
        if (name.includes('%')) name = decodeURIComponent(name)
      } catch {
      }
      if (!name || name.endsWith('/')) continue
      ;(out[bot] ??= new Set()).add(name)
    }
  }
  return Object.fromEntries(Object.entries(out).map(([k, v]) => [k, [...v].sort()]))
}

export function orderFiles(files: OutboxFile[]): OutboxFile[] {
  return [...files].sort((a, b) => b.modified - a.modified || a.name.localeCompare(b.name))
}

export function lastSettledTurnKey(turns: Record<string, Pick<Turn, 'id' | 'status' | 'completed_at'>> | undefined): string {
  let best: { id: string; at: string } | null = null
  for (const t of Object.values(turns ?? {})) {
    if (!t.completed_at || t.status === 'in_flight' || t.status === 'queued') continue
    if (!best || t.completed_at > best.at) best = { id: t.id, at: t.completed_at }
  }
  return best ? `${best.id}:${best.at}` : ''
}

export function isPreviewableImage(name: string): boolean {
  return /\.(png|jpe?g|gif|webp)$/i.test(name)
}

export const INLINE_PREVIEW_EXTENSIONS: readonly string[] = [
  'pdf',
  'png',
  'jpg',
  'jpeg',
  'gif',
  'webp',
  'txt',
  'log',
  'md',
  'csv',
  'tsv',
  'json',
]

export function isBrowserPreviewable(name: string): boolean {
  const m = /\.([a-z0-9]+)$/i.exec(name)
  if (!m) return false
  const ext = m[1].toLowerCase()
  return INLINE_PREVIEW_EXTENSIONS.includes(ext)
}

export function previewPlacement(
  anchor: { left: number; top: number; bottom: number },
  box: { width: number; height: number },
  viewport: { width: number; height: number },
  gap = 8,
): { left: number; top: number } {
  const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(v, Math.max(lo, hi)))
  if (anchor.left - gap - box.width >= gap) {
    return { left: anchor.left - gap - box.width, top: clamp(anchor.top, gap, viewport.height - gap - box.height) }
  }
  return {
    left: clamp(anchor.left, gap, viewport.width - gap - box.width),
    top: clamp(anchor.bottom + gap, gap, viewport.height - gap - box.height),
  }
}
