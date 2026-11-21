import { tokensLower } from './composerCost'
import { t } from './i18n'

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
