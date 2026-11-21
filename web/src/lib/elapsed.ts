export function fmtElapsed(ms: number): string {
  const total = Math.floor(ms / 1000)
  const s = total % 60
  const m = Math.floor(total / 60)
  if (m < 1) return `${s}s`
  if (m < 60) return `${m}m${String(s).padStart(2, '0')}`
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
}

export function activityStartedAt(
  agentStatusSince: string | null | undefined,
  turns: Iterable<{ status: string; created_at: string }>,
): string | null {
  if (agentStatusSince) return agentStatusSince
  let start: string | null = null
  for (const t of turns) {
    if (t.status === 'in_flight' && (!start || t.created_at < start)) start = t.created_at
  }
  return start
}
