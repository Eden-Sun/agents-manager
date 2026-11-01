import type { Bot, Project } from '../api/types'

/**
 * 這顆 bot 設定的自動壓縮門檻（token），跟 daemon `runtime_policy::effective` 同一個順序：
 * bot 覆寫 → 池條目 → 角色 policy。沒設、kind 不支援、或改了還沒重啟（`role_pending`，跑的還是舊值）＝null。
 * 只換狀態列的分母：CLI 回報的 context_size 是模型真實視窗，門檻比視窗小時才有意義。
 */
export function compactThreshold(bot: Pick<Bot, 'kind' | 'role' | 'runtime_override' | 'pool_entry' | 'role_pending'> | undefined, project: Pick<Project, 'role_policy' | 'executor_pool'> | undefined): number | null {
  if (!bot || (bot.kind !== 'claude' && bot.kind !== 'codex') || bot.role_pending) return null
  const pool = bot.pool_entry ? project?.executor_pool?.find((e) => e.id === bot.pool_entry)?.context_tokens : null
  const role = bot.role ? project?.role_policy?.[bot.role]?.context_tokens : null
  const v = bot.runtime_override?.context_tokens ?? pool ?? role ?? null
  return typeof v === 'number' && v > 0 ? v : null
}
