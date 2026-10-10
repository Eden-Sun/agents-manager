import { tokensLower } from './composerCost'

/**
 * context 一行字。有百分比：`40%（12k / 200k）`；只有 token 數（agy：視窗大小沒有可靠來源，daemon 不填百分比）：`約 12k tokens`——
 * 不編百分比；兩個都沒有＝沒有這一行。
 */
export function contextLabel(
  status: { context_used_pct: number | null; context_used_tokens: number | null; context_size: number | null } | null | undefined,
): string | null {
  if (!status) return null
  if (status.context_used_pct != null) {
    return `${Math.round(status.context_used_pct)}%${
      status.context_used_tokens != null && status.context_size != null
        ? `（${tokensLower(status.context_used_tokens)} / ${tokensLower(status.context_size)}）`
        : ''
    }`
  }
  return status.context_used_tokens != null && status.context_used_tokens > 0 ? `約 ${tokensLower(status.context_used_tokens)} tokens` : null
}
