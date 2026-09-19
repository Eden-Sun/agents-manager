/**
 * 預覽模式的 API（issue #253）：頂層 bot 的專案 vite dev server。
 * 三個端點都回同一個形狀；`GET /api/state` 的 `bot.preview` 只帶 status／port。
 */
import { rawTransport } from './index'
import { ApiError } from './types'
import { isRec, optStr, pick } from './normalize'

export type PreviewStatus = 'off' | 'starting' | 'running' | 'failed'

export interface Preview {
  status: PreviewStatus
  port: number | null
  dir: string | null
  pane_id: string | null
  error: string | null
  started_at: string | null
  /** v2：`attached` = 接上本機已在跑的 vite（斷開不會 kill 它）；`spawned` = AG Man 自己起的；舊 daemon 沒有＝null。 */
  source: PreviewSource | null
  pid: number | null
  /** v2：這顆 bot 的 vite 目錄候選（依序）。 */
  candidates: string[]
  /** v2：別份 checkout 已在跑的 vite；不自動接。 */
  others: PreviewOther[]
}

export type PreviewSource = 'spawned' | 'attached'
export interface PreviewOther {
  port: number
  dir: string
  pid: number | null
}

export const PREVIEW_OFF: Preview = {
  status: 'off',
  port: null,
  dir: null,
  pane_id: null,
  error: null,
  started_at: null,
  source: null,
  pid: null,
  candidates: [],
  others: [],
}

const posInt = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) && v > 0 ? Math.floor(v) : null)

const STATUSES: readonly PreviewStatus[] = ['off', 'starting', 'running', 'failed']

export function toPreviewStatus(v: unknown): PreviewStatus {
  return typeof v === 'string' && (STATUSES as readonly string[]).includes(v) ? (v as PreviewStatus) : 'off'
}

export function toPreview(v: unknown): Preview {
  if (!isRec(v)) return PREVIEW_OFF
  const port = pick(v, 'port')
  return {
    status: toPreviewStatus(pick(v, 'status')),
    port: typeof port === 'number' && Number.isFinite(port) && port > 0 ? Math.floor(port) : null,
    dir: optStr(pick(v, 'dir')),
    pane_id: optStr(pick(v, 'pane_id')),
    error: optStr(pick(v, 'error')),
    started_at: optStr(pick(v, 'started_at')),
    source: (() => {
      const x = pick(v, 'source')
      return x === 'spawned' || x === 'attached' ? x : null
    })(),
    pid: posInt(pick(v, 'pid')),
    candidates: (() => {
      const x = pick(v, 'candidates')
      return Array.isArray(x) ? x.filter((d): d is string => typeof d === 'string' && d !== '') : []
    })(),
    others: (() => {
      const x = pick(v, 'others')
      if (!Array.isArray(x)) return []
      return x.flatMap((o) => {
        if (!isRec(o)) return []
        const port = posInt(pick(o, 'port'))
        const dir = optStr(pick(o, 'dir'))
        return port && dir ? [{ port, dir, pid: posInt(pick(o, 'pid')) }] : []
      })
    })(),
  }
}

/** WS `preview_changed {bot_id, status, port}`：只帶狀態與 port，dir／error 沿用上一份（離開 failed 就清錯誤）。 */
export function toPreviewEvent(data: unknown, prev: Preview): Preview {
  const n = toPreview(data)
  const status = n.status
  return {
    ...prev,
    status,
    port: status === 'off' ? null : (n.port ?? prev.port),
    error: status === 'failed' ? prev.error : null,
    ...(status === 'off' ? { pane_id: null, started_at: null, source: null, pid: null } : {}),
  }
}

const path = (botId: string) => `/bots/${encodeURIComponent(botId)}/preview`

/** 404／405：這顆 daemon 還沒有預覽 API（POST 掉進前端 SPA 的 catch-all，那條只收 GET 所以是 405）。 */
export function previewApiMissing(e: unknown): boolean {
  return e instanceof ApiError && (e.status === 404 || e.status === 405)
}

export const PREVIEW_API_MISSING = 'daemon 還沒有預覽功能（二進位比前端舊），需要重建＋重啟 daemon。'

export async function fetchPreview(botId: string): Promise<Preview> {
  return toPreview(await rawTransport.request('GET', path(botId)))
}

/** v2 body：`auto`（預設，同目錄有就接、沒有就起）／`attach`（要帶 port）／`spawn`（可帶 dir 挑候選）。 */
export interface StartPreviewOpts {
  mode?: 'auto' | 'attach' | 'spawn'
  port?: number
  dir?: string
}

export async function startPreview(botId: string, opts?: StartPreviewOpts): Promise<Preview> {
  return toPreview(await rawTransport.request('POST', path(botId), opts))
}

export async function stopPreview(botId: string): Promise<Preview> {
  return toPreview(await rawTransport.request('DELETE', path(botId)))
}

/**
 * iframe 網址。不帶 token：預覽的若是 AG Man 自己的 dev UI，它靠 vite proxy 打 `/api/session`
 * 自己拿（daemon 對 loopback 對端不需 token），見 UI-DECISIONS〈預覽模式〉。
 */
export function previewUrl(port: number): string {
  return `http://${location.hostname}:${port}/`
}

