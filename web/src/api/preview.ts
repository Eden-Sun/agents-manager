/**
 * 預覽模式的 API（issue #253）：頂層 bot 的專案本機 dev server（vite／Next／…）。
 * 三個端點都回同一個形狀；`GET /api/state` 的 `bot.preview` 只帶 status／port。
 */
import { rawTransport } from './index'
import { ApiError } from './types'
import type { ApiErrorBody } from './types'
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
  /**
   * v6：daemon 的 `allow_lan`（issue #527）。`false` ＝它起的 dev server 釘在 loopback（#434／#452），
   * 只有跟 daemon 同一台機器的瀏覽器連得到；`null` ＝舊 daemon 沒這一格＝不知道，不下結論。
   */
  lan: boolean | null
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
  lan: null,
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
    lan: typeof pick(v, 'lan') === 'boolean' ? (pick(v, 'lan') as boolean) : null,
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

/** 偵測不到能起的 dev server：v1 叫 `no_vite_config`，v4 擴大後可能改名。 */
export const NO_DEV_REASONS = new Set(['no_vite_config', 'no_dev_server', 'no_dev_command'])

/**
 * 預覽的 409 講成人話（issue #524）。daemon 的 `LcError::conflict` 組出來的 body 是
 * `{error:"conflict", reason, …extra}`——**沒有 `message` 欄**，而 `ApiError` 的 message 會直接拿
 * `reason`，所以不接手的話畫面上就是一個英文代碼（`bot_not_running`、`stale_selection`…）。
 * store 的 `reasonText` 那套要 `body.message` 才給得出人話，這裡只能自己對。
 *
 * 認不出來的 reason 回 `null`，由呼叫端退回「代碼（HTTP 狀態）」——至少看得到代碼。
 */
export function previewReasonText(reason: string, body: ApiErrorBody = {}): string | null {
  const num = (k: string) => (typeof body[k] === 'number' ? (body[k] as number) : null)
  const port = num('port')
  const pid = num('pid')
  const at = port ? `:${port}` : '那個 port'
  switch (reason) {
    case 'not_top_level':
      return '只有頂層 bot 能開預覽。'
    case 'remote_host': {
      const host = typeof body.host === 'string' && body.host ? `（這顆 bot 在 ${body.host}）` : ''
      return `預覽只能開在跑 daemon 的這台機器上${host}。`
    }
    case 'bot_not_running':
      return '這顆 bot 沒在跑：dev server 要開在它的 pane 裡，先啟動 bot 再按一次。'
    case 'stale_selection':
      return `${at} 已經換成別的行程${pid ? `（pid ${pid}）` : ''}，沒有接上去——清單是稍早抓的。重新整理再挑一次。`
    case 'not_vite':
      return `${at} 上已經沒有 dev server 了（多半剛剛關掉）。重新整理清單再挑一次。`
    case 'no_free_port':
      return '預覽保留的那段 port 都被占著，挑不到可用的；關掉幾個 dev server 再試。'
    case 'preview_stop_failed':
      return '上一顆預覽的 pane 關不掉，所以這次沒有重開。到「終端」看看那個 pane，或稍後再試。'
    default:
      return NO_DEV_REASONS.has(reason)
        ? '這顆 bot 的目錄裡找不到可以起的 dev server（vite 設定檔或 package.json 的 dev script），AG Man 沒辦法自己起。'
        : null
  }
}

export async function fetchPreview(botId: string): Promise<Preview> {
  return toPreview(await rawTransport.request('GET', path(botId)))
}

/** v2 body：`auto`（預設，同目錄有就接、沒有就起）／`attach`（要帶 port）／`spawn`（可帶 dir 挑候選）。 */
export interface StartPreviewOpts {
  mode?: 'auto' | 'attach' | 'spawn'
  port?: number
  dir?: string
  /** `attach`：清單上那顆的 pid；帶了 daemon 會核對，port 已經換人就 409 `stale_selection`。 */
  pid?: number
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

const LOOPBACK_HOSTS = new Set(['localhost', '127.0.0.1', '[::1]', '::1', ''])

/** 這個頁面是從跑 daemon 的那台機器自己開的嗎。 */
export function isLoopbackHost(hostname: string): boolean {
  return LOOPBACK_HOSTS.has(hostname.toLowerCase())
}

/**
 * 這個瀏覽器連不連得到那顆 dev server（issue #527）。
 *
 * `allow_lan` 關著時 daemon 把自己起的 dev server 釘在 loopback（#434／#452），而 iframe 的網址是
 * `http://${location.hostname}:${port}/`——從手機或別台機器開的話那個 host 不是 loopback，一定連不到
 * （走 tailscale serve 的 https 頁面更是連請求都不會發：http 的 iframe 被當 mixed content 擋掉）。
 * `lan === null`（舊 daemon）＝不知道，不擋。
 */
export function previewOutOfReach(lan: boolean | null, hostname: string): boolean {
  return lan === false && !isLoopbackHost(hostname)
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
