import { FAST_TIER } from '../api/types'
import type { ModelInfo, PatchBotInput } from '../api/types'

export function modelSwitchPatch(
  bot: { effort: string | null; fast: boolean },
  models: readonly ModelInfo[],
  id: string,
  fromApi: boolean,
): PatchBotInput {
  const next = models.find((m) => m.id === id)
  const input: PatchBotInput = { model: id }
  if (bot.effort !== null && next && next.efforts.length > 0 && !next.efforts.includes(bot.effort)) {
    input.effort = next.default_effort
  }
  if (fromApi && bot.fast && next && !next.service_tiers.some((t) => t.id === FAST_TIER || t.id === 'fast')) input.fast = false
  return input
}
