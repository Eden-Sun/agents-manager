import type { Bot, Project } from '../api/types'

/**
 * 這顆 bot 設定的自動壓縮門檻（token），跟 daemon `runtime_policy::effective` 同一個順序：
 * bot 覆寫 → 角色 policy。沒設、kind 不支援、或改了還沒重啟（`role_pending`，跑的還是舊值）＝null。
 * 只換狀態列的分母：CLI 回報的 context_size 是模型真實視窗，門檻比視窗小時才有意義。
 */
export function compactThreshold(bot: Pick<Bot, 'kind' | 'role' | 'runtime_override' | 'role_pending' | 'policy_pending'> | undefined, project: Pick<Project, 'role_policy'> | undefined): number | null {
  // 只看「policy 改了還沒重啟」：`role_pending` 還含 shim 過期（每次部署都亮），拿它會讓分母無故退回 1M。舊 daemon 沒有 policy_pending 才退回 role_pending。
  if (!bot || (bot.kind !== 'claude' && bot.kind !== 'codex') || (bot.policy_pending ?? bot.role_pending)) return null
  const role = bot.role ? project?.role_policy?.[bot.role]?.context_tokens : null
  const v = bot.runtime_override?.context_tokens ?? role ?? null
  return typeof v === 'number' && v > 0 ? v : null
}
