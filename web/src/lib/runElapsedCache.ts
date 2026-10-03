const RUN_ELAPSED_CACHE_MAX = 300
const seenWorkingAt = new Map<string, number>()

export function runElapsedAt(botId: string): number | null {
  return seenWorkingAt.get(botId) ?? null
}

export function rememberRunElapsed(botId: string, at: number): number {
  const existing = seenWorkingAt.get(botId)
  if (existing !== undefined) return existing
  if (seenWorkingAt.size >= RUN_ELAPSED_CACHE_MAX) {
    const oldest = seenWorkingAt.keys().next().value
    if (oldest !== undefined) seenWorkingAt.delete(oldest)
  }
  seenWorkingAt.set(botId, at)
  return at
}

export function forgetRunElapsed(botId: string): void {
  seenWorkingAt.delete(botId)
}

export function runElapsedCacheSizeForTest(): number {
  return seenWorkingAt.size
}
