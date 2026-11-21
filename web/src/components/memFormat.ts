export function humanBytes(n: number): string {
  if (n <= 0) return '0'
  const g = n / 1024 ** 3
  const m = n / 1024 ** 2
  if (g >= 1 || Math.round(m) >= 1024) {
    const one = Math.round(g * 10) / 10
    return `${one < 10 ? g.toFixed(1) : Math.round(g)}G`
  }
  if (m >= 1 || Math.round(n / 1024) >= 1024) return `${Math.round(m)}M`
  return `${Math.max(1, Math.round(n / 1024))}K`
}
