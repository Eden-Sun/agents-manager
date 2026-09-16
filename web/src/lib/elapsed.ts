/** 14s / 3m14 / 1:20:05 — 帶秒並每秒上數（2026-09-16 使用者）。 */
export function fmtElapsed(ms: number): string {
  const total = Math.floor(ms / 1000)
  const s = total % 60
  const m = Math.floor(total / 60)
  if (m < 1) return `${s}s`
  if (m < 60) return `${m}m${String(s).padStart(2, '0')}`
  return `${Math.floor(m / 60)}:${String(m % 60).padStart(2, '0')}:${String(s).padStart(2, '0')}`
}
