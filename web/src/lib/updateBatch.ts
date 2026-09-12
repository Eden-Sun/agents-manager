import type { AgentStatus, Bot, Run } from '../api/types'

/**
 * 「claude 有更新 · 重啟 N 顆閒置的 Bot」那顆按鈕的數字（SPEC §6.9）。
 *
 * 真正決定要動哪幾顆的是 daemon（`daemon/src/bulk_restart.rs` 的 `plan`）——按下去那一刻的
 * 狀態才算數。這裡是同一條規則的前端副本，只負責**畫按鈕**：要不要出現、寫幾顆、順便說有幾顆
 * 在忙會被跳過。兩邊有出入時以 daemon 回來的計畫為準（送出後畫面就改用它的數字）。
 *
 * 規則刻意保守，跟 daemon 一字不差：只算 claude、只算真的帶著 `update_notice` 的 run，
 * 而且只有 `running` + `idle` 才進「可重啟」；`working` / `blocked` / `unknown` 與還有回合
 * 在飛的都進「在忙」。
 */

export interface UpdateBatchCounts {
  /** 帶著更新、現在就能重啟的。 */
  ready: { botId: string; name: string }[]
  /** 帶著更新、但正在忙所以會被跳過的。 */
  busy: { botId: string; name: string; why: string }[]
}

/** `agent_status` 對應到「為什麼現在不能動它」，`null` = 可以動。 */
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
    // team 成員由 team 排程管——daemon 那邊同樣不碰。子 agent 進來：它在自己的 pane 裡
    // exit + resume（2026-09-12 使用者：子 agent 全被跳過，更新永遠套不上去）。
    if (bot.managed_by === 'team') continue
    const run = runs[bot.id]
    if (!run || !run.update_notice?.trim()) continue
    const why = busyReason(run, hasInFlightTurn(bot.id))
    if (why) busy.push({ botId: bot.id, name: bot.name, why })
    else ready.push({ botId: bot.id, name: bot.name })
  }
  return { ready, busy }
}
