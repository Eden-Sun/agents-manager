export function updateRange(
  kind: string,
  notice: string | null | undefined,
  running: string | null | undefined,
): { from: string | null; to: string | null } {
  const from = running || null
  if (kind !== 'codex' || !notice) return { from, to: null }
  const [a = null, b = null] = notice.match(/\d+(?:\.\d+)+/g) ?? []
  if (notice.includes('這個 run 跑的是') && a && b) return { from: b, to: a }
  if (a && b) return { from: a, to: b }
  return { from, to: a }
}
