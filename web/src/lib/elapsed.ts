/** 14s / 3m14 / 1:20:05 — 帶秒並每秒上數（2026-09-16 使用者）。 */
export function fmtElapsed(ms: number): string {
  const total = Math.floor(ms / 1000)
  const s = total % 60
  const m = Math.floor(total / 60)
  if (m < 1) return `${s}s`
  if (m < 60) return `${m}m${String(s).padStart(2, '0')}`
  return `${Math.floor(m / 60)}:${String(m % 60).padStart(2, '0')}:${String(s).padStart(2, '0')}`
}

/**
 * 「正在跑」的起點，daemon 觀察到的優先，前端絕不自己發明（issue #93）。
 *
 * `agentStatusSince`：daemon 認得這次連續 working 從什麼時候開始——pane 狀態真的改變才會蓋，同一行
 * 狀態重複出現不算，也不會被回合中間的 AskUserQuestion 打斷拉長。沒有這欄時（升級前的舊列、或
 * 這個 run 還沒有任何一次真的狀態轉換）才退回這個回合最早那筆 in_flight turn 的 `created_at`——
 * 那也是 daemon 的紀錄，只是比較粗。兩個都沒有時回 `null`，由呼叫端決定要不要用本地時間墊底，
 * 並講清楚這只是這個網頁自己看到的、不是 daemon 的紀錄。
 */
export function activityStartedAt(
  agentStatusSince: string | null | undefined,
  turns: Iterable<{ status: string; created_at: string }>,
): string | null {
  if (agentStatusSince) return agentStatusSince
  let start: string | null = null
  for (const t of turns) {
    if (t.status === 'in_flight' && (!start || t.created_at < start)) start = t.created_at
  }
  return start
}
