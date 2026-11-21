import { formatGptShortModel } from './gptModel'

export function shortModel(kind: string, model: string | null): string | null {
  if (!model) return null
  if (kind === 'codex' || kind === 'openai-api') return formatGptShortModel(model)
  if (kind === 'agy') return model.replace(/^gemini-/, '').replace(AGY_EFFORT_SUFFIX, '')
  if (kind !== 'claude' && kind !== 'claude-api') return model
  const m = /^claude-(fable|opus|sonnet|haiku)(?:-\d.*)?$/.exec(model)
  return m ? m[1] : model
}

const AGY_EFFORT_SUFFIX = /-(low|medium|high|xhigh)$/i

export function agyEffort(kind: string, model: string | null): string | null {
  if (kind !== 'agy' || !model) return null
  return AGY_EFFORT_SUFFIX.exec(model)?.[1]?.toLowerCase() ?? null
}
