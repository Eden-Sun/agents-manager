/**
 * 設定頁「更新檢查」總覽（issue #1772）：把各處已有的更新資料整成每台主機一張表。
 * 全是純函式、不碰 store；資料來源都是既有的——`hosts[].herdr`／`tools`（工具探測）、`GET /api/upstream-updates`（上游版）、
 * `GET /api/integration-updates`（herdr integration）。動作也沿用既有 API，這裡只決定「這一列有沒有按鈕、按了送什麼」。
 */
import type { HerdrVersion, ToolMap, ToolStatus } from '../api/types'
import type { IntegrationHost, IntegrationRow } from '../api/integrationUpdates'
import type { UpstreamItem } from './upstreamUpdate'

export type CheckStatus = 'latest' | 'update' | 'missing' | 'unknown'
export type CheckGroup = 'herdr' | 'cli' | 'integration'

export type CheckAction =
  | { kind: 'cli'; cliKind: 'claude' | 'codex'; target: string }
  | { kind: 'herdr'; target: string }
  /** `fresh`：這家 integration 還沒裝，按下去是第一次安裝（grok／antigravity-cli 已登入時才有，#1871）。 */
  | { kind: 'integration'; name: string; fresh?: true }

export interface CheckRow {
  id: string
  group: CheckGroup
  label: string
  current: string | null
  latest: string | null
  status: CheckStatus
  /** 給人看的補充一句（為什麼不能按、為什麼查不到）。 */
  note: string | null
  action: CheckAction | null
}

export const STATUS_LABEL: Record<CheckStatus, string> = {
  latest: '最新',
  update: '可更新',
  missing: '未安裝',
  unknown: '查不到',
}

/** `herdr 0.9.5`、`2.1.300 (Claude Code)`、`codex-cli 0.157.0` → `0.9.5`／`2.1.300`／`0.157.0`；認不出來回 null。 */
export function cleanVersion(raw: string | null | undefined): string | null {
  const m = (raw ?? '').match(/\d+(?:\.\d+)+/)
  return m ? m[0] : null
}

/** a < b（逐段比，缺的段當 0）；任一邊認不出來回 null。 */
export function versionLess(a: string | null, b: string | null): boolean | null {
  if (!a || !b) return null
  const x = a.split('.').map(Number)
  const y = b.split('.').map(Number)
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    const p = x[i] ?? 0
    const q = y[i] ?? 0
    if (p !== q) return p < q
  }
  return false
}

const CLI_LABEL: Record<string, string> = { claude: 'Claude CLI', codex: 'Codex CLI', grok: 'Grok CLI', agy: 'Antigravity（agy）CLI' }
const INTEGRATION_LABEL: Record<string, string> = {
  claude: 'integration：claude',
  codex: 'integration：codex',
  'antigravity-cli': 'integration：antigravity-cli',
  grok: 'integration：grok',
}

function hostEntry(item: UpstreamItem | undefined, host: string) {
  return item?.hosts.find((h) => h.host === host) ?? null
}

function herdrRows(host: string, herdr: HerdrVersion | undefined, item: UpstreamItem | undefined, blockedWhy: string | null): CheckRow[] {
  const cli = cleanVersion(herdr?.cli_version)
  const server = cleanVersion(herdr?.server_version)
  const latest = item?.latest ?? null
  const entry = hostEntry(item, host)
  const behind = entry ? entry.behind : versionLess(cli, latest)
  const target = item?.target ?? latest
  // 還沒讀到 herdr（undefined）是查不到，不是未安裝；只有讀到了、版本欄位是空的才算未安裝。
  const cliStatus: CheckStatus = herdr === undefined ? 'unknown' : !herdr.cli_version ? 'missing' : latest === null ? 'unknown' : behind === true ? 'update' : behind === false ? 'latest' : 'unknown'
  const cliRow: CheckRow = {
    id: `${host}:herdr-cli`,
    group: 'herdr',
    label: 'herdr CLI',
    current: cli ?? (herdr?.cli_version || null),
    latest,
    status: cliStatus,
    note:
      cliStatus === 'update'
        ? blockedWhy ?? '更新會重啟 herdr server，所有 Bot 中斷約 1 分鐘後接回'
        : herdr === undefined
          ? '這台還沒讀到 herdr 版本'
          : cliStatus === 'unknown' && latest === null
            ? '查不到上游版本'
            : null,
    action: cliStatus === 'update' && target && !blockedWhy ? { kind: 'herdr', target } : null,
  }
  const serverBehind = versionLess(server, latest)
  const serverStatus: CheckStatus = !server ? 'unknown' : latest === null ? 'unknown' : serverBehind === true ? 'update' : 'latest'
  const serverRow: CheckRow = {
    id: `${host}:herdr-server`,
    group: 'herdr',
    label: 'herdr server',
    current: server,
    latest,
    status: serverStatus,
    note: !server ? '主機沒連上，讀不到 server 版本' : server && cli && server !== cli ? `server 與 CLI 不同版（CLI ${cli}），重啟 herdr server 後才換` : null,
    action: null,
  }
  return [cliRow, serverRow]
}

function cliRow(host: string, kind: string, tool: ToolStatus | undefined, item: UpstreamItem | undefined, updatable: boolean): CheckRow {
  const base = { id: `${host}:cli-${kind}`, group: 'cli' as const, label: CLI_LABEL[kind] ?? kind }
  // 工具清單還沒讀到（undefined）是查不到；讀到了、installed 為假才是未安裝。
  if (tool === undefined) return { ...base, current: null, latest: null, status: 'unknown', note: '這台還沒讀到工具清單', action: null }
  if (!tool.installed) return { ...base, current: null, latest: item?.latest ?? null, status: 'missing', note: null, action: null }
  const current = cleanVersion(tool.version) ?? tool.version
  // agy 沒有公開的版本來源：只列目前版本，狀態是「查不到」。
  if (kind === 'agy') return { ...base, current, latest: null, status: 'unknown', note: '沒有公開的版本來源，無法比對最新版', action: null }
  const entry = hostEntry(item, host)
  if (!item || item.latest === null) return { ...base, current, latest: null, status: 'unknown', note: '查不到上游版本', action: null }
  if (entry?.error) return { ...base, current, latest: item.latest, status: 'unknown', note: `讀不到這台的版本：${entry.error}`, action: null }
  const behind = entry ? entry.behind : versionLess(cleanVersion(current), item.target ?? item.latest)
  if (behind === null) return { ...base, current, latest: item.latest, status: 'unknown', note: null, action: null }
  if (!behind) return { ...base, current, latest: item.latest, status: 'latest', note: null, action: null }
  const target = item.target ?? item.latest
  if (updatable && (kind === 'claude' || kind === 'codex')) {
    return { ...base, current, latest: target, status: 'update', note: null, action: { kind: 'cli', cliKind: kind, target } }
  }
  return {
    ...base,
    current,
    latest: target,
    status: 'update',
    note: kind === 'grok' ? '沒有一鍵更新，請在那台跑 grok update' : null,
    action: null,
  }
}

/** integration 名 → 工具探測的 kind；只有這兩家已登入時能裝（daemon 自動裝一次、也可手動按，#1871）。 */
const LOGIN_KIND: Record<string, string> = { grok: 'grok', 'antigravity-cli': 'agy' }

/** daemon 自動裝過 grok／agy 但失敗（含裝完重讀狀態失敗，`error` 已寫明）：不管這一列現在是什麼狀態都要講原因與「不重試」。 */
function autoInstallFailure(row: IntegrationRow, retry: string): string | null {
  if (!(row.name in LOGIN_KIND) || row.auto_install?.ok !== false) return null
  return `自動安裝失敗：${row.auto_install.error ?? '原因不明'}；不重試，${retry}`
}

/** 沒裝的 integration 的說明。`loggedIn`：這台那家 CLI 已裝且已登入（工具探測）。 */
function notInstalledNote(row: IntegrationRow, loggedIn: boolean): string {
  if (!(row.name in LOGIN_KIND)) return '還沒安裝；這裡只顯示，不自動裝'
  const failed = autoInstallFailure(row, loggedIn ? '可按「安裝」再試，或重啟 daemon' : '重啟 daemon 才會再試')
  if (failed) return failed
  if (!loggedIn) return '還沒安裝；這台的 CLI 登入後 daemon 會自動裝一次，也才能手動安裝'
  return '還沒安裝；已登入，daemon 會在巡檢自動裝一次，也可以現在按「安裝」'
}

function integrationRow(host: string, row: IntegrationRow, tools: ToolMap | undefined): CheckRow {
  const t = (tools as Record<string, ToolStatus> | undefined)?.[LOGIN_KIND[row.name] ?? '']
  const loggedIn = t?.installed === true && t.logged_in === true
  const v = (n: number | null) => (n === null ? null : `v${n}`)
  const base = { id: `${host}:integration-${row.name}`, group: 'integration' as const, label: INTEGRATION_LABEL[row.name] ?? `integration：${row.name}` }
  switch (row.state) {
    case 'outdated':
      return { ...base, current: v(row.installed_version), latest: v(row.required_version), status: 'update', note: autoInstallFailure(row, '可按「更新」再試，或重啟 daemon'), action: { kind: 'integration', name: row.name } }
    case 'current':
      return { ...base, current: v(row.installed_version), latest: v(row.required_version), status: 'latest', note: null, action: null }
    case 'not_installed':
      return { ...base, current: null, latest: null, status: 'missing', note: notInstalledNote(row, loggedIn), action: loggedIn ? { kind: 'integration', name: row.name, fresh: true } : null }
    default:
      return { ...base, current: null, latest: null, status: 'unknown', note: [autoInstallFailure(row, '重啟 daemon 才會再試'), row.detail].filter(Boolean).join('；') || null, action: null }
  }
}

export interface HostCheck {
  host: string
  /** integration 那一段讀不到的原因（`rows` 裡的 integration 仍是上一次成功的）。 */
  integrationError: string | null
  /** 還沒收到這台的 integration 快照（daemon 第一輪還沒跑完、或舊 daemon）。 */
  integrationPending: boolean
  rows: CheckRow[]
}

export interface HostCheckInput {
  host: string
  tools: ToolMap | undefined
  herdr: HerdrVersion | undefined
  upstream: Record<string, UpstreamItem>
  integration: IntegrationHost | null
  /** herdr 這台不能從這裡更新的原因（遠端、共用 session）；null＝可以。 */
  herdrBlocked: string | null
}

export function buildHostCheck(input: HostCheckInput): HostCheck {
  const { host, tools, herdr, upstream, integration } = input
  const rows: CheckRow[] = [...herdrRows(host, herdr, upstream.herdr, input.herdrBlocked)]
  for (const kind of ['claude', 'codex', 'grok', 'agy']) {
    rows.push(cliRow(host, kind, (tools as Record<string, ToolStatus> | undefined)?.[kind], upstream[kind], true))
  }
  for (const row of integration?.items ?? []) rows.push(integrationRow(host, row, tools))
  return { host, integrationError: integration?.error ?? null, integrationPending: integration === null, rows }
}

/** 這台有幾項可更新（表頭的小計）。 */
export function updatableCount(rows: CheckRow[]): number {
  return rows.filter((r) => r.status === 'update').length
}

export interface StatusCounts {
  latest: number
  update: number
  missing: number
  unknown: number
}

export function statusCounts(rows: CheckRow[]): StatusCounts {
  const c: StatusCounts = { latest: 0, update: 0, missing: 0, unknown: 0 }
  for (const r of rows) c[r.status]++
  return c
}

/**
 * 表頭的一句話（#1813）：只有每一列都有版本、而且都是最新（integration 也讀到了）才說「都是最新」。
 * 未安裝、查不到的不算最新，照數量寫出來；integration 還沒讀到或讀失敗也要講，不能因為它不在列裡就當成沒事。
 */
export function summaryText(check: HostCheck): string {
  const { rows } = check
  // buildHostCheck 一定給 herdr 與四家 CLI 共 6 列，正常不會是空的；`rows.length > 0` 只是擋住空陣列被誤判成「都是最新」。
  const c = statusCounts(rows)
  const unloaded = check.integrationPending ? 'integration 還沒讀到' : check.integrationError ? 'integration 讀取失敗' : null
  if (rows.length > 0 && c.latest === rows.length && unloaded === null) return '都是最新'
  const parts = [
    c.update > 0 ? `${c.update} 項可更新` : null,
    c.latest > 0 ? `${c.latest} 項最新` : null,
    c.missing > 0 ? `${c.missing} 項未安裝` : null,
    c.unknown > 0 ? `${c.unknown} 項查不到` : null,
    unloaded,
  ]
  return parts.filter((p) => p !== null).join('、')
}
