export function leftPct(usedPct: number): number {
  const left = Math.max(0, 100 - usedPct)
  return left < 10 ? Math.round(left * 10) / 10 : Math.round(left)
}
