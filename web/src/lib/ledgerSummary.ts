import type { LedgerItem, LedgerItemStatus } from '../api/types'

/** 各狀態的件數（不含已併入的；併入的已經算到別項）。 */
export function ledgerSummary(items: readonly LedgerItem[]): Partial<Record<LedgerItemStatus, number>> {
  const out: Partial<Record<LedgerItemStatus, number>> = {}
  for (const it of items) {
    if (it.status === 'merged') continue
    out[it.status] = (out[it.status] ?? 0) + 1
  }
  return out
}
