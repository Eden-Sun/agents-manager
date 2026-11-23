import { formatGptShortModel } from './gptModel'

const CLAUDE_ABBR: Record<string, string> = {
  opus: 'opu',
  sonnet: 'son',
  haiku: 'hai',
  fable: 'fab',
}

export function shortModel(kind: string, model: string | null): string | null {
  if (!model) return null
  if (kind === 'codex' || kind === 'openai-api') return formatGptShortModel(model)
  if (kind === 'agy') return model.replace(/^gemini-/, '').replace(AGY_EFFORT_SUFFIX, '')
  if (kind !== 'claude' && kind !== 'claude-api') return model
  const clean = model.replace(/\[[^\]]+\]$/, '')
  const m = /^(?:claude-)?(fable|opus|sonnet|haiku)(?:-(\d+)(?:[.-](\d+))?(?:-\d{8})?)?$/i.exec(clean)
  if (!m) return clean
  const abbr = CLAUDE_ABBR[m[1].toLowerCase()]
  if (!m[2]) return abbr
  return m[3] ? `${abbr}-${m[2]}.${m[3]}` : `${abbr}-${m[2]}`
}

const AGY_EFFORT_SUFFIX = /-(low|medium|high|xhigh)$/i

export function agyEffort(kind: string, model: string | null): string | null {
  if (kind !== 'agy' || !model) return null
  return AGY_EFFORT_SUFFIX.exec(model)?.[1]?.toLowerCase() ?? null
}
