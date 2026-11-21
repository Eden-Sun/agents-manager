import type { QuotaWindow } from '../api/types'

export function carriedOverAt(w: Pick<QuotaWindow, 'observed_at'> | null | undefined, updatedAt: string | null | undefined): string | null {
  const observed = w?.observed_at
  if (!observed) return null
  const at = Date.parse(observed)
  const rec = Date.parse(updatedAt ?? '')
  if (Number.isNaN(at) || Number.isNaN(rec)) return null
  return at < rec ? observed : null
}
