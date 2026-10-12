/** 1.4G / 820M / 64M — 一格寬度就要看得懂，所以個位數才給小數。 */
export function humanBytes(n: number): string {
  if (n <= 0) return '0'
  const g = n / 1024 ** 3
  const m = n / 1024 ** 2
  // 先看四捨五入後落在哪單位：1023.6M 進位是 1024，要寫成 1.0G（不寫 1024M）。
  if (g >= 1 || Math.round(m) >= 1024) {
    // 9.96G 進位到 10：≥10 的寫法是整數，不寫 10.0G。
    const one = Math.round(g * 10) / 10
    return `${one < 10 ? g.toFixed(1) : Math.round(g)}G`
  }
  if (m >= 1) return `${Math.round(m)}M`
  return `${Math.max(1, Math.round(n / 1024))}K`
}
