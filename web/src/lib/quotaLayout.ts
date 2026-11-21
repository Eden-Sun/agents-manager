export type QuotaFit = 0 | 1 | 2

export interface QuotaFitState {
  level: QuotaFit
  need: [number | null, number | null]
}

export const QUOTA_FIT_START: QuotaFitState = { level: 0, need: [null, null] }

const SLACK = 0.5

export function withFreshNeed(s: QuotaFitState, full: number): QuotaFitState {
  if (s.level === 0) return s
  return { level: s.level, need: [full, full] }
}

export function nextQuotaFit(s: QuotaFitState, avail: number, content: number, hasHostTag: boolean): QuotaFitState {
  if (s.level !== 2 && content > avail + SLACK) {
    const need: QuotaFitState['need'] = [...s.need]
    need[s.level] = content
    if (s.level === 0 && !hasHostTag) need[1] = content
    const level: QuotaFit = s.level === 0 && hasHostTag ? 1 : 2
    return { level, need }
  }
  if (s.level > 0) {
    const down = (s.level - 1) as 0 | 1
    const target = down === 1 && !hasHostTag ? 0 : down
    const need = s.need[target]
    if (need === null || avail + SLACK >= need) return { level: target, need: s.need }
  }
  return s
}
