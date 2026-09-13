import type { AgentStatus, Bot, Run } from '../api/types'

/**
 * 「重啟 N 顆閒置的 Bot」按鈕的數字（SPEC §6.9）。只畫按鈕用的前端副本，須與 daemon
 * `bulk_restart.rs` 的 `plan` 規則一致；實際以 daemon 回的計畫為準。
 */

export interface UpdateBatchCounts {
  ready: { botId: string; name: string }[]
  /** 正在忙會被跳過的。 */
  busy: { botId: string; name: string; why: string }[]
}

/** `null` = 可以動。 */
function busyReason(run: Run, hasInFlightTurn: boolean): string | null {
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
    if (bot.kind !== 'claude') continue
    // 子 agent 也算（2026-09-12 使用者：子 agent 全被跳過，更新永遠套不上去）。
    const run = runs[bot.id]
    if (!run || !run.update_notice?.trim()) continue
    const why = busyReason(run, hasInFlightTurn(bot.id))
    if (why) busy.push({ botId: bot.id, name: bot.name, why })
    else ready.push({ botId: bot.id, name: bot.name })
  }
  return { ready, busy }
}
