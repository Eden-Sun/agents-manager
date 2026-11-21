import type { LedgerItem, LedgerItemStatus } from '../api/types'

export function ledgerSummary(items: readonly LedgerItem[]): Partial<Record<LedgerItemStatus, number>> {
  const out: Partial<Record<LedgerItemStatus, number>> = {}
  for (const it of items) {
    if (it.status === 'merged') continue
    out[it.status] = (out[it.status] ?? 0) + 1
  }
  return out
}
