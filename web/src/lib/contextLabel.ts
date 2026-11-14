import { tokensLower } from './composerCost'
import { t } from './i18n'

/**
 * context 一行字。有百分比：`40%（12k / 200k）`（agy 的百分比由 daemon 用 `[agy] context_window_tokens` 算好寫進 status）；只有 token 數（舊的 agy 紀錄、沒有視窗大小）：`約 12k tokens`——
 * 不編百分比；兩個都沒有＝沒有這一行。
 */
export function contextLabel(
  status: { context_used_pct: number | null; context_used_tokens: number | null; context_size: number | null } | null | undefined,
): string | null {
  if (!status) return null
  if (status.context_used_pct != null) {
    return `${Math.round(status.context_used_pct)}%${
      status.context_used_tokens != null && status.context_size != null
        ? t('（{used} / {size}）', { used: tokensLower(status.context_used_tokens), size: tokensLower(status.context_size) })
        : ''
    }`
  }
  return status.context_used_tokens != null && status.context_used_tokens > 0 ? t('約 {n} tokens', { n: tokensLower(status.context_used_tokens) }) : null
}
