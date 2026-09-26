import type { AgentStatus, Bot, Run } from '../api/types'
import { cmpVersion } from './releaseTriage'
import { updateRange } from './updateRange'

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
  ready: { botId: string; name: string }[]
  /** 正在忙、或需要先手動處理才會被跳過的。`install`＝codex 新版還沒裝（header 另一顆 chip 負責，見 `codexInstallPlan`）。 */
  busy: { botId: string; name: string; why: string; install?: true }[]
}

/** codex 的新版還沒裝：重啟換不到，只能先手動安裝（daemon 的 `Skip::NeedsManualInstall`）。 */
export function needsManualInstall(bot: Bot, run: Run | null | undefined): boolean {
  return bot.kind === 'codex' && Boolean(run?.update_notice?.includes('需安裝'))
}

/** `null` = 可以動。 */
function busyReason(bot: Bot, run: Run, hasInFlightTurn: boolean): string | null {
  if (needsManualInstall(bot, run)) return '新版還沒裝，要先手動安裝才能套用'
  return runBusyReason(run, hasInFlightTurn)
}

/** 不管更新有沒有裝，這顆現在能不能重啟。 */
function runBusyReason(run: Run, hasInFlightTurn: boolean): string | null {
  if (run.state !== 'running') return '還在啟動或關閉中'
  const st: AgentStatus = run.agent_status
  if (st === 'working') return '正在跑'
  if (st === 'blocked') return '卡在提問，等人回答'
  if (st !== 'idle') return '狀態不明'
  if (hasInFlightTurn) return '還有一回合沒收掉'
  return null
}

export function updateBatchCounts(
  bots: Bot[],
  runs: Record<string, Run | null>,
  hasInFlightTurn: (botId: string) => boolean,
): UpdateBatchCounts {
  const ready: UpdateBatchCounts['ready'] = []
  const busy: UpdateBatchCounts['busy'] = []
  for (const bot of bots) {
    // update_notice 只有 claude／codex 會被 update_watch 寫（grok 沒有這條巡邏），跟寫入端假設對齊。
    if (bot.kind !== 'claude' && bot.kind !== 'codex') continue
    // 子 agent 也算（2026-09-12 使用者：子 agent 全被跳過，更新永遠套不上去）。
    const run = runs[bot.id]
    if (!run || !run.update_notice?.trim()) continue
    const why = busyReason(bot, run, hasInFlightTurn(bot.id))
    if (why && needsManualInstall(bot, run)) busy.push({ botId: bot.id, name: bot.name, why, install: true })
    else if (why) busy.push({ botId: bot.id, name: bot.name, why })
    else ready.push({ botId: bot.id, name: bot.name })
  }
  return { ready, busy }
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

/**
 * header「安裝 codex 新版」那顆 chip 的內容（SPEC §6.9，daemon `cli_update.rs`）：取第一台還有「需安裝」codex 的主機，
 * 列出那台裝好之後的一鍵重啟會動到哪幾顆——daemon 的批次範圍是「那台主機的 codex」，前端照同一條切。
 * 沒有需安裝的 codex 回 `null`。多台都有時一次一台，裝完那台 chip 自然換到下一台。
 */
export function codexInstallPlan(
  bots: Bot[],
  runs: Record<string, Run | null>,
  hasInFlightTurn: (botId: string) => boolean,
  hostOf: (bot: Bot) => string,
): CodexInstallPlan | null {
  const first = bots.find((b) => needsManualInstall(b, runs[b.id]))
  if (!first) return null
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
    // 子 agent 批次一律跳過（daemon `Skip::Child`，SPEC §6.5a），由父 bot 用 herdr 重開。
    const child = bot.managed_by === 'child' || Boolean(bot.parent_bot_id)
    const why = child ? '子 agent，由父 Bot 重開' : runBusyReason(run, hasInFlightTurn(bot.id))
    if (why) plan.busy.push({ botId: bot.id, name: bot.name, why })
    else plan.ready.push({ botId: bot.id, name: bot.name })
  }
  return plan
}

/**
 * header 的兩顆更新 chip 要不要合成一顆（UI-DECISIONS「header 的 codex 安裝」）：只有手機、而且兩顆都會出現時。
 * 手機名字行在 390px 只讓得出一顆圖示的寬（兩顆並排時長名字只剩 ▾）；桌機的額度列放得下兩顆，照舊並排。
 */
export function mergeUpdateChips(phone: boolean, restartShown: boolean, codexShown: boolean): boolean {
  return phone && restartShown && codexShown
}
