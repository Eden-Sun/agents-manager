import type { AgentStatus, Bot, Run } from '../api/types'
import { cmpVersion } from './releaseTriage'
import { updateRange } from './updateRange'
import { backgroundLabel } from './backgroundJobs'
import { t } from './i18n'
import type { UpstreamItem } from '../store/upstreamUpdate'

export interface UpdateBatchCounts {
  ready: { botId: string; name: string; backgroundUnknown?: true }[]
  busy: { botId: string; name: string; why: string; install?: true }[]
  passive: { botId: string; name: string; why: string }[]
}

export function needsManualInstall(bot: Bot, run: Run | null | undefined): boolean {
  return (bot.kind === 'codex' || bot.kind === 'claude') && Boolean(run?.update_notice?.includes('需安裝'))
}

export function isChildBot(bot: Pick<Bot, 'managed_by' | 'parent_bot_id'>): boolean {
  return bot.managed_by === 'child' || Boolean(bot.parent_bot_id)
}

function neverRestartedReason(bot: Bot, run: Run): string | null {
  if (isChildBot(bot)) return t('子 agent，由父 Bot 重開')
  if (bot.herdr_session === 'default' || run.herdr_session === 'default') return t('在你自己的 herdr default session 裡，daemon 不動它')
  return null
}

export function passiveUpdateHint(bot: Bot, run: Run | null | undefined): string | null {
  if (!run?.update_notice?.trim()) return null
  if (isChildBot(bot)) return needsManualInstall(bot, run) ? t('新版還沒安裝，由父 Bot 安排安裝與重啟') : t('新版已安裝，由父 Bot 安排重啟')
  if (needsManualInstall(bot, run)) return null
  if (neverRestartedReason(bot, run)) return t('新版已安裝；這顆在你自己的 herdr default session 裡，daemon 不會幫你重啟')
  return null
}

function busyReason(bot: Bot, run: Run, hasInFlightTurn: boolean): string | null {
  if (needsManualInstall(bot, run)) return t('新版還沒裝，要先手動安裝才能套用')
  return runBusyReason(run, hasInFlightTurn)
}

function runBusyReason(run: Run, hasInFlightTurn: boolean): string | null {
  if (run.state !== 'running') return t('還在啟動或關閉中')
  const st: AgentStatus = run.agent_status
  if (st === 'working') return t('正在跑')
  if (st === 'blocked') return t('卡在提問，等人回答')
  if (st !== 'idle') return t('狀態不明')
  if (hasInFlightTurn) return t('還有一回合沒收掉')
  const bg = run.background_jobs ?? 0
  if (bg > 0) return backgroundLabel(bg)
  return null
}

export function readyLabel(b: UpdateBatchCounts['ready'][number]): string {
  return b.backgroundUnknown ? t('{name}（背景狀態未知）', { name: b.name }) : b.name
}

export function updateBatchCounts(
  bots: Bot[],
  runs: Record<string, Run | null>,
  hasInFlightTurn: (botId: string) => boolean,
): UpdateBatchCounts {
  const ready: UpdateBatchCounts['ready'] = []
  const busy: UpdateBatchCounts['busy'] = []
  const passive: UpdateBatchCounts['passive'] = []
  for (const bot of bots) {
    if (bot.kind !== 'claude' && bot.kind !== 'codex') continue
    const run = runs[bot.id]
    if (!run || !run.update_notice?.trim()) continue
    const never = needsManualInstall(bot, run) ? null : neverRestartedReason(bot, run)
    if (never) {
      passive.push({ botId: bot.id, name: bot.name, why: never })
      continue
    }
    const why = busyReason(bot, run, hasInFlightTurn(bot.id))
    if (why && needsManualInstall(bot, run)) busy.push({ botId: bot.id, name: bot.name, why, install: true })
    else if (why) busy.push({ botId: bot.id, name: bot.name, why })
    else ready.push({ botId: bot.id, name: bot.name, ...(run.background_jobs === null ? { backgroundUnknown: true as const } : {}) })
  }
  return { ready, busy, passive }
}

export interface CodexInstallPlan {
  host: string
  notice: string
  installCount: number
  ready: { botId: string; name: string }[]
  busy: { botId: string; name: string; why: string }[]
}

export interface ClaudeInstallPlan {
  target: string
  hosts: { host: string; installedVersion: string | null; error: string | null; behind: boolean }[]
  installHosts: string[]
}

export function claudeInstallPlan(item: UpstreamItem | null | undefined): ClaudeInstallPlan | null {
  if (!item || item.autoUpdate || item.kind !== 'claude' || !item.hasUpdate || !item.target || item.hosts.length === 0) return null
  const hosts = item.hosts.map((host) => ({ ...host }))
  return {
    target: item.target,
    hosts,
    installHosts: hosts.filter((host) => host.behind || host.error !== null).map((host) => host.host),
  }
}

export function codexInstallPlan(
  bots: Bot[],
  runs: Record<string, Run | null>,
  hasInFlightTurn: (botId: string) => boolean,
  hostOf: (bot: Bot) => string,
  upstream?: UpstreamItem | null,
): CodexInstallPlan | null {
  if (upstream?.autoUpdate) return null
  const first = bots.find((b) => b.kind === 'codex' && needsManualInstall(b, runs[b.id]))
  if (!first) return codexUpstreamPlan(bots, runs, hasInFlightTurn, hostOf, upstream)
  const host = hostOf(first)
  const plan: CodexInstallPlan = { host, notice: runs[first.id]?.update_notice ?? '', installCount: 0, ready: [], busy: [] }
  for (const bot of bots) {
    const run = runs[bot.id]
    if (bot.kind !== 'codex' || !run || !run.update_notice?.trim() || hostOf(bot) !== host) continue
    if (needsManualInstall(bot, run)) {
      plan.installCount += 1
      const to = updateRange('codex', run.update_notice, null).to
      const cur = updateRange('codex', plan.notice, null).to
      if (to && (!cur || cmpVersion(to, cur) > 0)) plan.notice = run.update_notice
    }
    const why = neverRestartedReason(bot, run) ?? runBusyReason(run, hasInFlightTurn(bot.id))
    if (why) plan.busy.push({ botId: bot.id, name: bot.name, why })
    else plan.ready.push({ botId: bot.id, name: bot.name })
  }
  return plan
}

function codexUpstreamPlan(
  bots: Bot[],
  runs: Record<string, Run | null>,
  hasInFlightTurn: (botId: string) => boolean,
  hostOf: (bot: Bot) => string,
  upstream: UpstreamItem | null | undefined,
): CodexInstallPlan | null {
  if (!upstream || upstream.kind !== 'codex' || !upstream.hasUpdate || !upstream.target) return null
  const behind = upstream.hosts.filter((h) => h.behind)
  const disk = behind.find((h) => h.host === 'local') ?? behind[0]
  if (!disk) return null
  const notice = disk.installedVersion
    ? t('codex 有新版 {from} → {to}，需安裝後重啟', { from: disk.installedVersion, to: upstream.target })
    : t('codex 有新版 {to}，需安裝後重啟', { to: upstream.target })
  const plan: CodexInstallPlan = { host: disk.host, notice, installCount: 0, ready: [], busy: [] }
  for (const bot of bots) {
    const run = runs[bot.id]
    if (bot.kind !== 'codex' || !run || run.state !== 'running' || hostOf(bot) !== disk.host) continue
    const why = neverRestartedReason(bot, run) ?? runBusyReason(run, hasInFlightTurn(bot.id))
    if (why) plan.busy.push({ botId: bot.id, name: bot.name, why })
    else plan.ready.push({ botId: bot.id, name: bot.name })
  }
  return plan
}

export function mergeUpdateChips(
  phone: boolean,
  restartShown: boolean,
  codexShown: boolean,
  claudeShown = false,
  herdrShown = false,
  grokShown = false,
  agyShown = false,
): boolean {
  return phone && Number(restartShown) + Number(codexShown) + Number(claudeShown) + Number(herdrShown) + Number(grokShown) + Number(agyShown) > 1
}
