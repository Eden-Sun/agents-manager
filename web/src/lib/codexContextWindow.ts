
export const GPT_61_SOL_CONTEXT_WINDOW = 500_000

export function isGpt61Sol(model: string | null | undefined): boolean {
  return (model ?? '').trim().toLowerCase() === 'gpt-6.1-sol'
}

export function effectiveModelOf(runtimeModel: string | null | undefined, botModel: string | null | undefined): string | null {
  const runtime = runtimeModel?.trim()
  if (runtime) return runtime
  const configured = botModel?.trim()
  return configured || null
}

export function solWindowApplies(kind: string | null | undefined, model: string | null | undefined): boolean {
  return kind === 'codex' && isGpt61Sol(model)
}

export function sol500kContext(
  kind: string | null | undefined,
  model: string | null | undefined,
  usedTokens: number | null | undefined,
): { pct: number; tokens: number } | null {
  if (!solWindowApplies(kind, model)) return null
  if (usedTokens == null || !Number.isFinite(usedTokens) || usedTokens < 0) return null
  return { pct: (usedTokens / GPT_61_SOL_CONTEXT_WINDOW) * 100, tokens: usedTokens }
}
