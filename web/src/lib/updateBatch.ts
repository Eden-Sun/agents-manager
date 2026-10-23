import type { AgentStatus, Bot, Run } from '../api/types'
import { cmpVersion } from './releaseTriage'
import { updateRange } from './updateRange'
import { backgroundLabel } from './backgroundJobs'
import { t } from './i18n'
import type { UpstreamItem } from '../store/upstreamUpdate'

/**
 * 「重啟 N 顆閒置的 Bot」按鈕的數字（SPEC §6.9）。只畫按鈕用的前端副本，須與 daemon
 * `bulk_restart.rs` 的 `plan` 規則一致；實際以 daemon 回的計畫為準。
 *
 * **codex 也算，但只有「磁碟已裝好」才進 ready**（2026-09-22）：codex 的更新通知有兩種文案
 * （`codex_update.rs`）——已經裝好、這個 run 還跑舊版（notice 含「已安裝」）跟 claude 一樣重啟就換；
 * 新版**還沒安裝**（notice 含「需安裝」）重啟一顆沒裝新版的 codex 換不到任何東西，daemon 端的
 * `bulk_restart::Skip::NeedsManualInstall` 也是同一條界線——留在候選名單（header 才看得到），
 * 但算進 busy、講清楚原因，不是像以前那樣整個 kind 被濾掉、在 header 上完全消失。
 */

export interface UpdateBatchCounts {
  /** `backgroundUnknown`：巡邏還沒看過它的背景工作（daemon 剛重啟、新 run）——沒有證據所以不擋，但確認框要標出來（#767）。 */
  ready: { botId: string; name: string; backgroundUnknown?: true }[]
  /** 正在忙、或需要先手動處理才會被跳過的。`install`＝codex 新版還沒裝（header 另一顆 chip 負責，見 `codexInstallPlan`）。 */
  busy: { botId: string; name: string; why: string; install?: true }[]
  /**
   * 一鍵重啟永遠不動的（子 agent、default session；`neverRestartedReason`）：不是「暫時在忙」，所以不進 `ready`／`busy`、
   * 不算 header 灰色 ⇑N（#1911）；資訊留在那一列的被動提示（`passiveUpdateHint`）。
   */
  passive: { botId: string; name: string; why: string }[]
}

/** codex 的新版還沒裝：重啟換不到，只能先手動安裝（daemon 的 `Skip::NeedsManualInstall`）。 */
export function needsManualInstall(bot: Bot, run: Run | null | undefined): boolean {
  return (bot.kind === 'codex' || bot.kind === 'claude') && Boolean(run?.update_notice?.includes('需安裝')) // i18n-ignore
}

/**
 * 子 agent：由父 bot 用 herdr 重開，daemon 的 `/api/bots/{id}/restart` 一律回 409（`refuse_child_restart`，SPEC §6.5a）。
 * 前端所有「重啟」入口（更新徽章、⟳ 設定落差、設定面板橫幅）與一鍵重啟計數共用這一條判斷（#1911）。
 */
export function isChildBot(bot: Pick<Bot, 'managed_by' | 'parent_bot_id'>): boolean {
  return bot.managed_by === 'child' || Boolean(bot.parent_bot_id)
}

/**
 * 不是「等一下就好」、而是一鍵重啟**永遠不動**的（daemon `bulk_restart::skip_reason` 的 `Child`／`DefaultSession`）：
 * 子 agent 由父 bot 用 herdr 重開（SPEC §6.5a，2026-09-22）；使用者自己 herdr default session 裡的 pane daemon 只觀察（SPEC §6.5.1）。
 * 前端的計數要跟 daemon 同一條線，不然框裡寫「重啟 N 顆」，實際動的比它少。
 */
function neverRestartedReason(bot: Bot, run: Run): string | null {
  if (isChildBot(bot)) return t('子 agent，由父 Bot 重開')
  if (bot.herdr_session === 'default' || run.herdr_session === 'default') return t('在你自己的 herdr default session 裡，daemon 不動它')
  return null
}

/**
 * 那一列自己的被動提示（側欄 ⌃⌃ 與 context bar「升級」的 tooltip）：這顆不在一鍵重啟的範圍裡，資訊不隨 header 計數消失（#1911）。
 * 子 agent 一律沒有重啟按鈕（`isChildBot`）；新版還沒裝（通知寫「需安裝」）時不說「已安裝」。`null`＝這顆是一般 bot，沒有額外要說的。
 */
export function passiveUpdateHint(bot: Bot, run: Run | null | undefined): string | null {
  if (!run?.update_notice?.trim()) return null
  if (isChildBot(bot)) return needsManualInstall(bot, run) ? t('新版還沒安裝，由父 Bot 安排安裝與重啟') : t('新版已安裝，由父 Bot 安排重啟')
  if (needsManualInstall(bot, run)) return null
  if (neverRestartedReason(bot, run)) return t('新版已安裝；這顆在你自己的 herdr default session 裡，daemon 不會幫你重啟')
  return null
}

/** `null` = 可以動。 */
function busyReason(bot: Bot, run: Run, hasInFlightTurn: boolean): string | null {
  if (needsManualInstall(bot, run)) return t('新版還沒裝，要先手動安裝才能套用')
  return runBusyReason(run, hasInFlightTurn)
}

/** 不管更新有沒有裝，這顆現在能不能重啟。 */
function runBusyReason(run: Run, hasInFlightTurn: boolean): string | null {
  if (run.state !== 'running') return t('還在啟動或關閉中')
  const st: AgentStatus = run.agent_status
  if (st === 'working') return t('正在跑')
  if (st === 'blocked') return t('卡在提問，等人回答')
  if (st !== 'idle') return t('狀態不明')
  if (hasInFlightTurn) return t('還有一回合沒收掉')
  // 回合結束了、背景還在跑：重啟一退 CLI，背景 shell／終端跟著沒了（daemon `Skip::BackgroundJobs`，#767）。null＝沒看過，不擋。
  const bg = run.background_jobs ?? 0
  if (bg > 0) return backgroundLabel(bg)
  return null
}

/** 確認框裡「會重啟」那一列的字：背景狀態沒看過的標出來（#767）。 */
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
    // update_notice 只有 claude／codex 會被 update_watch 寫（grok 沒有這條巡邏），跟寫入端假設對齊。
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
  /** 那台「需安裝」通知裡目標最新的一則：版本區間從這裡讀（`updateRange`），跟 daemon 核對的目標同一個（#569）。 */
  notice: string
  /** 那台還寫著「需安裝」的 codex 有幾顆。 */
  installCount: number
  /** 裝好之後會被重啟的（那台閒置的 codex，含已經是「已安裝」的）。 */
  ready: { botId: string; name: string }[]
  /** 裝好之後仍會被跳過的（在忙），之後再按一般的 ⌃⌃。 */
  busy: { botId: string; name: string; why: string }[]
}

export interface ClaudeInstallPlan {
  target: string
  /** 快照列出的每台已安裝 Claude 主機，確認框都會顯示目前版本與共同目標。 */
  hosts: { host: string; installedVersion: string | null; error: string | null; behind: boolean }[]
  /** 只安裝落後或讀不到版本的主機；已在目標版的主機保留在清單但不重裝。 */
  installHosts: string[]
}

/** Claude fleet 快照有任何主機未到共同目標時持續提供安裝計畫；不依賴一次性 toast 或 active run。 */
export function claudeInstallPlan(item: UpstreamItem | null | undefined): ClaudeInstallPlan | null {
  if (!item || item.kind !== 'claude' || !item.hasUpdate || !item.target || item.hosts.length === 0) return null
  const hosts = item.hosts.map((host) => ({ ...host }))
  return {
    target: item.target,
    hosts,
    installHosts: hosts.filter((host) => host.behind || host.error !== null).map((host) => host.host),
  }
}

/**
 * header「安裝 codex 新版」那顆 chip 的內容（SPEC §6.9，daemon `cli_update.rs`）：取第一台還有「需安裝」codex 的主機，
 * 列出那台裝好之後的一鍵重啟會動到哪幾顆——daemon 的批次範圍是「那台主機的 codex」，前端照同一條切。
 * 沒有在跑的 codex 帶「需安裝」時，退回上游快照（`GET /api/upstream-updates` 的 codex，2026-09-29 使用者：
 * 上游有新版就該有安裝鈕，不必等有 codex 在跑）：取第一台落後的主機（本機優先），通知文字照 daemon 的格式合成。
 * 都沒有回 `null`。多台都有時一次一台，裝完那台 chip 自然換到下一台。
 */
export function codexInstallPlan(
  bots: Bot[],
  runs: Record<string, Run | null>,
  hasInFlightTurn: (botId: string) => boolean,
  hostOf: (bot: Bot) => string,
  upstream?: UpstreamItem | null,
): CodexInstallPlan | null {
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
    // 子 agent、default session 的批次一律跳過（daemon `Skip::Child`／`DefaultSession`）。
    const why = neverRestartedReason(bot, run) ?? runBusyReason(run, hasInFlightTurn(bot.id))
    if (why) plan.busy.push({ botId: bot.id, name: bot.name, why })
    else plan.ready.push({ botId: bot.id, name: bot.name })
  }
  return plan
}

/** 沒有在跑的 codex 帶「需安裝」時，照上游快照開一份（見 `codexInstallPlan`）。那台在跑的 codex 照樣列進重啟／跳過。 */
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
  // 同 daemon `codex_update::pending_text`：`updateRange` 從這句讀 (from, to]。
  const notice = disk.installedVersion
    ? t('codex 有新版 {from} → {to}，需安裝後重啟', { from: disk.installedVersion, to: upstream.target })
    : t('codex 有新版 {to}，需安裝後重啟', { to: upstream.target })
  const plan: CodexInstallPlan = { host: disk.host, notice, installCount: 0, ready: [], busy: [] }
  for (const bot of bots) {
    const run = runs[bot.id]
    if (bot.kind !== 'codex' || !run || run.state !== 'running' || hostOf(bot) !== disk.host) continue
    // 跟 `codexInstallPlan` 同一條線：子 agent 與 default session 都不動（daemon `Skip::Child`／`DefaultSession`）。
    const why = neverRestartedReason(bot, run) ?? runBusyReason(run, hasInFlightTurn(bot.id))
    if (why) plan.busy.push({ botId: bot.id, name: bot.name, why })
    else plan.ready.push({ botId: bot.id, name: bot.name })
  }
  return plan
}

/**
 * header 的兩顆更新 chip 要不要合成一顆（UI-DECISIONS「header 的 codex 安裝」）：只有手機、而且兩顆都會出現時。
 * 手機名字行在 390px 只讓得出一顆圖示的寬（兩顆並排時長名字只剩 ▾）；桌機的額度列放得下兩顆，照舊並排。
 */
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
