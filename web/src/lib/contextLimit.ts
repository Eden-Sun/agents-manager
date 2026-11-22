import type { Bot, Project } from '../api/types'

/** 與 daemon `am-config::runtime_params::default_context_tokens` 同值：agy 沒設時的自動壓縮門檻。 */
const AGY_DEFAULT_CONTEXT_TOKENS = 400000

export function compactThreshold(bot: Pick<Bot, 'kind' | 'role' | 'runtime_override' | 'role_pending' | 'policy_pending'> | undefined, project: Pick<Project, 'role_policy'> | undefined): number | null {
  if (!bot || (bot.kind !== 'claude' && bot.kind !== 'codex' && bot.kind !== 'agy') || (bot.policy_pending ?? bot.role_pending)) return null
  const role = bot.role ? project?.role_policy?.[bot.role]?.context_tokens : null
  const v = bot.runtime_override?.context_tokens ?? role ?? (bot.kind === 'agy' ? AGY_DEFAULT_CONTEXT_TOKENS : null)
  return typeof v === 'number' && v > 0 ? v : null
}
