import type { AgentStatus, Bot, Run } from '../api/types'

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
  /** 正在忙、或需要先手動處理才會被跳過的。 */
  busy: { botId: string; name: string; why: string }[]
}

/** codex 的新版還沒裝：重啟換不到，只能先手動安裝（daemon 的 `Skip::NeedsManualInstall`）。 */
export function needsManualInstall(bot: Bot, run: Run | null | undefined): boolean {
  return bot.kind === 'codex' && Boolean(run?.update_notice?.includes('需安裝'))
}

/** `null` = 可以動。 */
function busyReason(bot: Bot, run: Run, hasInFlightTurn: boolean): string | null {
  if (needsManualInstall(bot, run)) return '新版還沒裝，要先手動安裝才能套用'
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
    if (why) busy.push({ botId: bot.id, name: bot.name, why })
    else ready.push({ botId: bot.id, name: bot.name })
  }
  return { ready, busy }
}
