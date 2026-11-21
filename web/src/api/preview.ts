import { msg, t, tr } from "../lib/i18n"
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
  source: PreviewSource | null
  pid: number | null
  candidates: PreviewCandidate[]
  command: string | null
  others: PreviewOther[]
  lan: boolean | null
}

export interface PreviewCandidate {
  dir: string
  command: string | null
}

export type PreviewSource = 'spawned' | 'attached'
export type PreviewRelation = 'same_dir' | 'same_repo' | 'other'
export interface PreviewOther {
  port: number
  dir: string
  pid: number | null
  relation: PreviewRelation
  kind: string
  repo: string | null
}

const RELATION_ORDER: readonly PreviewRelation[] = ['same_dir', 'same_repo', 'other']

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
        const relation: PreviewRelation = rel === 'same_dir' || rel === 'same_repo' || rel === 'other' ? rel : 'same_repo'
        const kind = optStr(pick(o, 'kind')) ?? 'vite'
        return port && dir ? [{ port, dir, pid: posInt(pick(o, 'pid')), relation, kind, repo: optStr(pick(o, 'repo')) }] : []
      })
    })(),
  }
}

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

export function previewApiMissing(e: unknown): boolean {
  return e instanceof ApiError && (e.status === 404 || e.status === 405)
}

export const PREVIEW_API_MISSING = msg('daemon 還沒有預覽功能（二進位比前端舊），需要重建＋重啟 daemon。')

export const NO_DEV_REASONS = new Set(['no_vite_config', 'no_dev_server', 'no_dev_command'])

export function previewReasonText(reason: string, body: ApiErrorBody = {}): string | null {
  const num = (k: string) => (typeof body[k] === 'number' ? (body[k] as number) : null)
  const port = num('port')
  const pid = num('pid')
  const at = port ? `:${port}` : t('那個 port')
  switch (reason) {
    case 'not_top_level':
      return t('只有頂層 bot 能開預覽。')
    case 'remote_host': {
      const host = typeof body.host === 'string' && body.host ? t('（這顆 bot 在 {host}）', { host: body.host }) : ''
      return t('預覽只能開在跑 daemon 的這台機器上{host}。', { host })
    }
    case 'bot_not_running':
      return t('這顆 bot 沒在跑：dev server 要開在它的 pane 裡，先啟動 bot 再按一次。')
    case 'stale_selection':
      const pidStr = pid ? t('（pid {pid}）', { pid }) : ''
      return t('{at} 已經換成別的行程{pid}，沒有接上去——清單是稍早抓的。重新整理再挑一次。', { at, pid: pidStr })
    case 'not_vite':
      return t('{at} 上已經沒有 dev server 了（多半剛剛關掉）。重新整理清單再挑一次。', { at })
    case 'no_free_port':
      return t('預覽保留的那段 port 都被占著，挑不到可用的；關掉幾個 dev server 再試。')
    case 'preview_stop_failed':
      return t('上一顆預覽的 pane 關不掉，所以這次沒有重開。到「終端」看看那個 pane，或稍後再試。')
    default:
      return NO_DEV_REASONS.has(reason)
        ? t('這顆 bot 的目錄裡找不到可以起的 dev server（vite 設定檔或 package.json 的 dev script），AG Man 沒辦法自己起。')
        : null
  }
}

export async function fetchPreview(botId: string): Promise<Preview> {
  return toPreview(await rawTransport.request('GET', path(botId)))
}

export interface StartPreviewOpts {
  mode?: 'auto' | 'attach' | 'spawn'
  port?: number
  dir?: string
  pid?: number
}

export async function startPreview(botId: string, opts?: StartPreviewOpts): Promise<Preview> {
  return toPreview(await rawTransport.request('POST', path(botId), opts))
}

export async function stopPreview(botId: string): Promise<Preview> {
  return toPreview(await rawTransport.request('DELETE', path(botId)))
}

export function previewUrl(port: number): string {
  return `http://${location.hostname}:${port}/`
}

const LOOPBACK_HOSTS = new Set(['localhost', '127.0.0.1', '[::1]', '::1', ''])

export function isLoopbackHost(hostname: string): boolean {
  return LOOPBACK_HOSTS.has(hostname.toLowerCase())
}

export function previewOutOfReach(lan: boolean | null, hostname: string): boolean {
  return lan === false && !isLoopbackHost(hostname)
}

export function previewMixedContent(protocol: string, hostname: string): boolean {
  return protocol === 'https:' && !isLoopbackHost(hostname)
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
  unknown: msg('其他'),
}

export function kindLabel(kind: string): string {
  const k = kind.trim().toLowerCase()
  const raw = KIND_LABEL[k]
  if (raw) return tr(raw)
  return k ? k[0].toUpperCase() + k.slice(1) : tr(KIND_LABEL.unknown)
}
