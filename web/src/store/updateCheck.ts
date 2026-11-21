import type { HerdrVersion, ToolMap, ToolStatus } from '../api/types'
import type { IntegrationHost, IntegrationRow } from '../api/integrationUpdates'
import type { UpstreamItem } from './upstreamUpdate'
import { msg, t } from '../lib/i18n'

export type CheckStatus = 'latest' | 'update' | 'missing' | 'unknown'
export type CheckGroup = 'herdr' | 'cli' | 'integration'

export type CheckAction =
  | { kind: 'cli'; cliKind: 'claude' | 'codex'; target: string }
  | { kind: 'herdr'; target: string }
  | { kind: 'agy'; target: string }
  | { kind: 'integration'; name: string; fresh?: true }

export interface CheckRow {
  id: string
  group: CheckGroup
  label: string
  current: string | null
  latest: string | null
  status: CheckStatus
  note: string | null
  action: CheckAction | null
}

export const STATUS_LABEL: Record<CheckStatus, string> = {
  latest: msg('最新'),
  update: msg('可更新'),
  missing: msg('未安裝'),
  unknown: msg('查不到'),
}

export function cleanVersion(raw: string | null | undefined): string | null {
  const m = (raw ?? '').match(/\d+(?:\.\d+)+/)
  return m ? m[0] : null
}

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

function cliLabel(kind: string): string {
  if (kind === 'agy') return t('Antigravity（agy）CLI')
  return ({ claude: 'Claude CLI', codex: 'Codex CLI', grok: 'Grok CLI' } as Record<string, string>)[kind] ?? kind
}

function integrationLabel(name: string): string {
  return t('integration：{name}', { name })
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
        ? blockedWhy ?? t('更新會重啟 herdr server，所有 Bot 中斷約 1 分鐘後接回')
        : herdr === undefined
          ? t('這台還沒讀到 herdr 版本')
          : cliStatus === 'unknown' && latest === null
            ? t('查不到上游版本')
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
    note: !server ? t('主機沒連上，讀不到 server 版本') : server && cli && server !== cli ? t('server 與 CLI 不同版（CLI {cli}），重啟 herdr server 後才換', { cli }) : null,
    action: null,
  }
  return [cliRow, serverRow]
}

function cliRow(host: string, kind: string, tool: ToolStatus | undefined, item: UpstreamItem | undefined, updatable: boolean): CheckRow {
  const base = { id: `${host}:cli-${kind}`, group: 'cli' as const, label: cliLabel(kind) }
  if (tool === undefined) return { ...base, current: null, latest: null, status: 'unknown', note: t('這台還沒讀到工具清單'), action: null }
  if (!tool.installed) return { ...base, current: null, latest: item?.latest ?? null, status: 'missing', note: null, action: null }
  const current = cleanVersion(tool.version) ?? tool.version
  const entry = hostEntry(item, host)
  if (!item || item.latest === null) return { ...base, current, latest: null, status: 'unknown', note: kind === 'agy' ? t('查不到官方 manifest 的版本') : t('查不到上游版本'), action: null }
  if (entry?.error) return { ...base, current, latest: item.latest, status: 'unknown', note: t('讀不到這台的版本：{error}', { error: entry.error }), action: null }
  const behind = entry ? entry.behind : versionLess(cleanVersion(current), item.target ?? item.latest)
  if (behind === null) return { ...base, current, latest: item.latest, status: 'unknown', note: null, action: null }
  if (!behind) return { ...base, current, latest: item.latest, status: 'latest', note: null, action: null }
  const target = item.target ?? item.latest
  if (updatable && (kind === 'claude' || kind === 'codex')) {
    return { ...base, current, latest: target, status: 'update', note: null, action: { kind: 'cli', cliKind: kind, target } }
  }
  if (updatable && kind === 'agy') {
    return { ...base, current, latest: target, status: 'update', note: t('跑著的 agy bot 不會被打斷，重啟才換版'), action: { kind: 'agy', target } }
  }
  return {
    ...base,
    current,
    latest: target,
    status: 'update',
    note: kind === 'grok' ? t('沒有一鍵更新，請在那台跑 grok update') : null,
    action: null,
  }
}

const LOGIN_KIND: Record<string, string> = { grok: 'grok', 'antigravity-cli': 'agy' }

function autoInstallFailure(row: IntegrationRow, retry: string): string | null {
  if (!(row.name in LOGIN_KIND) || row.auto_install?.ok !== false) return null
  return t('自動安裝失敗：{error}；不重試，{retry}', { error: row.auto_install.error ?? t('原因不明'), retry })
}

function notInstalledNote(row: IntegrationRow, loggedIn: boolean): string {
  if (!(row.name in LOGIN_KIND)) return t('還沒安裝；這裡只顯示，不自動裝')
  const failed = autoInstallFailure(row, loggedIn ? t('可按「安裝」再試，或重啟 daemon') : t('重啟 daemon 才會再試'))
  if (failed) return failed
  if (!loggedIn) return t('還沒安裝；這台的 CLI 登入後 daemon 會自動裝一次，也才能手動安裝')
  return t('還沒安裝；已登入，daemon 會在巡檢自動裝一次，也可以現在按「安裝」')
}

function integrationRow(host: string, row: IntegrationRow, tools: ToolMap | undefined): CheckRow {
  const probe = (tools as Record<string, ToolStatus> | undefined)?.[LOGIN_KIND[row.name] ?? '']
  const loggedIn = probe?.installed === true && probe.logged_in === true
  const v = (n: number | null) => (n === null ? null : `v${n}`)
  const base = { id: `${host}:integration-${row.name}`, group: 'integration' as const, label: integrationLabel(row.name) }
  switch (row.state) {
    case 'outdated':
      return { ...base, current: v(row.installed_version), latest: v(row.required_version), status: 'update', note: autoInstallFailure(row, t('可按「更新」再試，或重啟 daemon')), action: { kind: 'integration', name: row.name } }
    case 'current':
      return { ...base, current: v(row.installed_version), latest: v(row.required_version), status: 'latest', note: null, action: null }
    case 'not_installed':
      return { ...base, current: null, latest: null, status: 'missing', note: notInstalledNote(row, loggedIn), action: loggedIn ? { kind: 'integration', name: row.name, fresh: true } : null }
    default:
      return { ...base, current: null, latest: null, status: 'unknown', note: [autoInstallFailure(row, t('重啟 daemon 才會再試')), row.detail].filter(Boolean).join(t('；')) || null, action: null }
  }
}

export interface HostCheck {
  host: string
  integrationError: string | null
  integrationPending: boolean
  rows: CheckRow[]
}

export interface HostCheckInput {
  host: string
  tools: ToolMap | undefined
  herdr: HerdrVersion | undefined
  upstream: Record<string, UpstreamItem>
  integration: IntegrationHost | null
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

export function summaryText(check: HostCheck): string {
  const { rows } = check
  const c = statusCounts(rows)
  const unloaded = check.integrationPending ? t('integration 還沒讀到') : check.integrationError ? t('integration 讀取失敗') : null
  if (rows.length > 0 && c.latest === rows.length && unloaded === null) return t('都是最新')
  const parts = [
    c.update > 0 ? t('{n} 項可更新', { n: c.update }) : null,
    c.latest > 0 ? t('{n} 項最新', { n: c.latest }) : null,
    c.missing > 0 ? t('{n} 項未安裝', { n: c.missing }) : null,
    c.unknown > 0 ? t('{n} 項查不到', { n: c.unknown }) : null,
    unloaded,
  ]
  return parts.filter((p) => p !== null).join(t('、'))
}
