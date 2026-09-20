import { FAST_TIER } from '../api/types'
import type { ModelInfo, PatchBotInput } from '../api/types'

/**
 * 標題列快速選單換模型要送的 patch。強度不適用新模型就換成新模型的預設；`fast` 也一樣：
 * 新模型沒有 priority tier 而這顆 bot 還開著 fast，codex 每次都會帶 `service_tier="priority"`（API.md §12.2），
 * 對不支援的模型等於送出一個不合法的設定。只認 API 清單（`fromApi`）：靜態退路沒有 tier 資訊，會誤清。
 */
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
  if (fromApi && bot.fast && next && !next.service_tiers.some((t) => t.id === FAST_TIER)) input.fast = false
  return input
}
