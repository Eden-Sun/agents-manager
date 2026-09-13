import type { Bot, Run } from '../api/types'

/** 有 active run 時 `DELETE /api/projects/{id}` 回 409（API.md），確認框事先停掉確認鈕；active 定義同 daemon。 */
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
