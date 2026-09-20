/**
 * 預覽模式的 API（issue #253）：頂層 bot 的專案本機 dev server（vite／Next／…）。
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
  /** v2：`attached` = 接上本機已在跑的 dev server（斷開不會 kill 它）；`spawned` = AG Man 自己起的；舊 daemon 沒有＝null。 */
  source: PreviewSource | null
  pid: number | null
  /** v2：這顆 bot 的 dev server 目錄候選（依序）；v4 起每筆可帶會用的指令。 */
  candidates: PreviewCandidate[]
  /** v4：實際跑的那一行指令（spawned）；舊 daemon 沒有＝null。 */
  command: string | null
  /** v2：本機在跑的 dev server（v3 起含別的 repo，帶 relation；v4 起帶 kind）。 */
  others: PreviewOther[]
}

export interface PreviewCandidate {
  dir: string
  /** v4：這個目錄會用的指令（`bun run dev`／`bunx vite`…）；沒有＝null。 */
  command: string | null
}

export type PreviewSource = 'spawned' | 'attached'
/** v3：`same_dir` = 這顆 bot 自己的目錄；`same_repo` = 同 repo 的別份 checkout；`other` = 別的專案。 */
export type PreviewRelation = 'same_dir' | 'same_repo' | 'other'
export interface PreviewOther {
  port: number
  dir: string
  pid: number | null
  relation: PreviewRelation
  /** v4：`vite`／`next`／`webpack`／…／`unknown`；舊 daemon 沒有欄位＝`vite`（那時只認 vite）。 */
  kind: string
  /** daemon 給的 repo 名（可能沒有）；分組標題退回目錄的最後一段。 */
  repo: string | null
}

const RELATION_ORDER: readonly PreviewRelation[] = ['same_dir', 'same_repo', 'other']

/** 依 relation 分組（固定順序，空組不出）。 */
export function groupOthers(others: PreviewOther[]): { relation: PreviewRelation; items: PreviewOther[] }[] {
  return RELATION_ORDER.map((relation) => ({ relation, items: others.filter((o) => o.relation === relation) })).filter(
    (g) => g.items.length > 0,
  )
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
  command: null,
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
      if (!Array.isArray(x)) return []
      return x.flatMap((c): PreviewCandidate[] => {
        if (typeof c === 'string') return c ? [{ dir: c, command: null }] : []
        if (!isRec(c)) return []
        const dir = optStr(pick(c, 'dir'))
        return dir ? [{ dir, command: optStr(pick(c, 'command')) }] : []
      })
    })(),
    command: optStr(pick(v, 'command')),
    others: (() => {
      const x = pick(v, 'others')
      if (!Array.isArray(x)) return []
      return x.flatMap((o) => {
        if (!isRec(o)) return []
        const port = posInt(pick(o, 'port'))
        const dir = optStr(pick(o, 'dir'))
        const rel = pick(o, 'relation')
        // v2 的 daemon 只列同 repo 的別份 checkout、沒有 relation：照 same_repo 看；v3 判不出來的自己會給 other。
        const relation: PreviewRelation = rel === 'same_dir' || rel === 'same_repo' || rel === 'other' ? rel : 'same_repo'
        const kind = optStr(pick(o, 'kind')) ?? 'vite'
        return port && dir ? [{ port, dir, pid: posInt(pick(o, 'pid')), relation, kind, repo: optStr(pick(o, 'repo')) }] : []
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
    ...(status === 'off' ? { pane_id: null, started_at: null, source: null, pid: null, command: null } : {}),
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


const KIND_LABEL: Record<string, string> = {
  vite: 'Vite',
  next: 'Next.js',
  webpack: 'webpack',
  astro: 'Astro',
  storybook: 'Storybook',
  nuxt: 'Nuxt',
  remix: 'Remix',
  rsbuild: 'Rsbuild',
  parcel: 'Parcel',
  angular: 'Angular',
  unknown: '其他',
}

/** 清單每筆的 kind 標籤；沒見過的名字照原樣首字大寫。 */
export function kindLabel(kind: string): string {
  const k = kind.trim().toLowerCase()
  return KIND_LABEL[k] ?? (k ? k[0].toUpperCase() + k.slice(1) : KIND_LABEL.unknown)
}
