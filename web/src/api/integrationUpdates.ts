/**
 * herdr integration 更新總覽（SPEC §6.9c，issue #1772）：
 * `GET /api/integration-updates`（每台主機最近一輪 `herdr integration status` 的快照）、
 * `POST /api/hosts/{name}/herdr-integration`（使用者按「更新」才在那台跑 `herdr integration install <name>`）。
 */
import { arr, isRec, pick, str } from './normalize'
import { ApiError } from './types'
import { rawTransport } from './index'

export type IntegrationState = 'current' | 'outdated' | 'not_installed' | 'unknown'

export interface IntegrationRow {
  name: string
  state: IntegrationState
  installed_version: number | null
  required_version: number | null
  path: string | null
  detail: string | null
  /** daemon 自動裝過這一列（grok／antigravity-cli 已登入時，#1871）；沒試過＝null。 */
  auto_install: { ok: boolean; error: string | null } | null
}

export interface IntegrationHost {
  host: string
  checked_at: string
  /** 這一輪讀不到的原因；`items` 還是上一次成功的。 */
  error: string | null
  items: IntegrationRow[]
}

const STATES: IntegrationState[] = ['current', 'outdated', 'not_installed', 'unknown']

function num(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null
}

function toAutoInstall(raw: unknown): IntegrationRow['auto_install'] {
  if (!isRec(raw)) return null
  return { ok: pick(raw, 'ok') === true, error: str(pick(raw, 'error')) || null }
}

export function toIntegrationHosts(raw: unknown): IntegrationHost[] {
  const hosts = isRec(raw) ? arr(pick(raw, 'hosts')) : []
  return hosts.flatMap((h) => {
    if (!isRec(h) || !str(pick(h, 'host'))) return []
    const items = arr(pick(h, 'items')).flatMap((x): IntegrationRow[] => {
      if (!isRec(x) || !str(pick(x, 'name'))) return []
      const state = str(pick(x, 'state')) as IntegrationState
      return [
        {
          name: str(pick(x, 'name')),
          state: STATES.includes(state) ? state : 'unknown',
          installed_version: num(pick(x, 'installed_version')),
          required_version: num(pick(x, 'required_version')),
          path: str(pick(x, 'path')) || null,
          detail: str(pick(x, 'detail')) || null,
          auto_install: toAutoInstall(pick(x, 'auto_install')),
        },
      ]
    })
    return [{ host: str(pick(h, 'host')), checked_at: str(pick(h, 'checked_at')), error: str(pick(h, 'error')) || null, items }]
  })
}

export async function fetchIntegrationUpdates(): Promise<IntegrationHost[]> {
  return toIntegrationHosts(await rawTransport.request('GET', '/integration-updates'))
}

export interface IntegrationInstallResult {
  host: string
  name: string
  /** 裝完重讀的實際狀態；讀不到＝null。 */
  state: IntegrationState | null
  output: string
  /** 裝完重讀狀態失敗的原因（此時 `state` 是 null，daemon 不拿舊資料當結果）。 */
  rereadError: string | null
}

export async function installIntegration(host: string, name: string): Promise<IntegrationInstallResult> {
  const raw = await rawTransport.request('POST', `/hosts/${encodeURIComponent(host)}/herdr-integration`, { name })
  const o = isRec(raw) ? raw : {}
  const state = str(pick(o, 'state')) as IntegrationState
  return {
    host: str(pick(o, 'host'), host),
    name: str(pick(o, 'name'), name),
    state: STATES.includes(state) ? state : null,
    output: str(pick(o, 'output')),
    rereadError: str(pick(o, 'reread_error')) || null,
  }
}

/** 409／403／404／502 → 給人看的一句；daemon 的 409 都帶寫好的 `message`。 */
export function integrationErrText(e: unknown, host: string): string {
  if (e instanceof ApiError) {
    const human = e.body.message
    if (typeof human === 'string' && human.trim()) return human.trim()
    if (e.status === 404 && !e.body.reason) return `這顆 daemon 還沒有 integration 更新（HTTP 404），或不認得主機 ${host}`
    if (e.status === 405) return '這顆 daemon 還沒有 integration 更新（HTTP 405），二進位比前端舊；要重建並重啟 daemon'
    if (e.status === 403) return 'integration 更新只給使用者在設定頁按'
    return `HTTP ${e.status}${typeof e.body.error === 'string' ? `：${e.body.error}` : ''}`
  }
  return e instanceof Error ? e.message : String(e)
}
