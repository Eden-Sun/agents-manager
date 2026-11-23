import type { Bot, Project } from '../api/types'

const DEFAULT_CONTEXT_TOKENS = 400000

export function compactThreshold(
  bot:
    | (Pick<Bot, 'kind' | 'role' | 'runtime_override' | 'role_pending' | 'policy_pending'> & {
        share_profile?: Bot['share_profile']
      })
    | undefined,
  project: Pick<Project, 'role_policy'> | undefined,
): number | null {
  if (!bot || (bot.kind !== 'claude' && bot.kind !== 'codex' && bot.kind !== 'agy') || (bot.policy_pending ?? bot.role_pending)) return null
  const role = bot.role ? project?.role_policy?.[bot.role]?.context_tokens : null
  const isDefault400k = bot.kind === 'agy' || bot.share_profile != null
  const v = bot.runtime_override?.context_tokens ?? role ?? (isDefault400k ? DEFAULT_CONTEXT_TOKENS : null)
  return typeof v === 'number' && v > 0 ? v : null
}
