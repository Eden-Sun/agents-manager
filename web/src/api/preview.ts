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
}

export const PREVIEW_OFF: Preview = { status: 'off', port: null, dir: null, pane_id: null, error: null, started_at: null }

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
    ...(status === 'off' ? { pane_id: null, started_at: null } : {}),
  }
}

const path = (botId: string) => `/bots/${encodeURIComponent(botId)}/preview`

export async function fetchPreview(botId: string): Promise<Preview> {
  try {
    return toPreview(await rawTransport.request('GET', path(botId)))
  } catch (e) {
    // 舊 daemon 還沒有這個端點：當作沒開過，面板照樣能顯示「啟動預覽」（按下去才會有真的錯誤）。
    if (e instanceof ApiError && (e.status === 404 || e.status === 405)) return PREVIEW_OFF
    throw e
  }
}

export async function startPreview(botId: string): Promise<Preview> {
  return toPreview(await rawTransport.request('POST', path(botId)))
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

