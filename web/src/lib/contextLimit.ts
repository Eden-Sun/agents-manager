import type { Bot, Project } from '../api/types'

export function compactThreshold(bot: Pick<Bot, 'kind' | 'role' | 'runtime_override' | 'role_pending' | 'policy_pending'> | undefined, project: Pick<Project, 'role_policy'> | undefined): number | null {
  if (!bot || (bot.kind !== 'claude' && bot.kind !== 'codex') || (bot.policy_pending ?? bot.role_pending)) return null
  const role = bot.role ? project?.role_policy?.[bot.role]?.context_tokens : null
  const v = bot.runtime_override?.context_tokens ?? role ?? null
  return typeof v === 'number' && v > 0 ? v : null
}
