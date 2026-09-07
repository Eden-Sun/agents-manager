import type { Bot, Run } from '../api/types'

/**
 * `DELETE /api/projects/{id}` 在任一 bot 還有 active run 時回 409（API.md）。刪除確認框
 * 事先把數字算出來、把確認鈕停掉，不必等 API 回錯。active 的定義跟 daemon 一致：
 * starting / running / stopping 都算「還在跑」。
 */
export function projectDeleteBlockers(
  bots: readonly Bot[],
  runs: Readonly<Record<string, Run | null | undefined>>,
  projectId: string,
): { total: number; active: number } {
  let total = 0
  let active = 0
  for (const b of bots) {
    if (b.project_id !== projectId) continue
    total++
    const run = runs[b.id]
    if (run && (run.state === 'starting' || run.state === 'running' || run.state === 'stopping')) active++
  }
  return { total, active }
}
