/** 1.4G / 820M / 64M — 一格寬度就要看得懂，所以個位數才給小數。 */
export function humanBytes(n: number): string {
  if (n <= 0) return '0'
  const g = n / 1024 ** 3
  if (g >= 1) return `${g < 10 ? g.toFixed(1) : Math.round(g)}G`
  const m = n / 1024 ** 2
  if (m >= 1) return `${Math.round(m)}M`
  return `${Math.max(1, Math.round(n / 1024))}K`
}
