import type { BotKind, ModelInfo } from '../api/types'
import {
  agyModelAllowed,
  CLAUDE_EFFORT_OPTIONS,
  CODEX_EFFORT_OPTIONS,
  EFFORT_OPTIONS,
  openAiApiEffortsFor,
} from '../api/types'
import { modelMatches, stripModelContext } from './modelMatches'

type Parsed = { series: string; version: number[] }

const toVersion = (s: string) => s.split(/[.-]/).map((x) => Number(x))

export function parseModelSeries(kind: BotKind, id: string): Parsed | null {
  const norm = stripModelContext(id)
  if (kind === 'codex') {
    const m = /^gpt-(\d+(?:\.\d+)*)-([a-z]+)$/.exec(norm)
    return m ? { series: `codex:${m[2]}`, version: toVersion(m[1]) } : null
  }
  if (kind === 'openai-api') {
    const m = /^gpt-(\d+(?:\.\d+)*)-([a-z]+)$/.exec(norm)
    return m ? { series: `openai:${m[2]}`, version: toVersion(m[1]) } : null
  }
  if (kind === 'claude' || kind === 'claude-api') {
    const alias = /^(fable|opus|sonnet|haiku)$/.exec(norm)
    if (alias) return { series: `claude:${alias[1]}`, version: [] }
    const m = /^claude-(fable|opus|sonnet|haiku)-(\d+(?:-\d+)*)$/.exec(norm)
    return m ? { series: `claude:${m[1]}`, version: toVersion(m[2]) } : null
  }
  if (kind === 'agy') {
    const m = /^gemini-(\d+(?:\.\d+)*)-([a-z]+)(?:-(?:low|medium|high|xhigh))?$/.exec(norm)
    return m ? { series: `agy:${m[2]}`, version: toVersion(m[1]) } : null
  }
  return null
}

export function compareVersion(a: number[], b: number[]): number {
  const n = Math.max(a.length, b.length)
  for (let i = 0; i < n; i++) {
    const d = (a[i] ?? 0) - (b[i] ?? 0)
    if (d !== 0) return d
  }
  return 0
}

export function latestModels(
  kind: BotKind,
  models: ModelInfo[],
  selected: string | null | undefined,
): { models: ModelInfo[]; oldIds: ReadonlySet<string> } {
  const best = new Map<string, number[]>()
  for (const m of models) {
    const p = parseModelSeries(kind, m.id)
    if (!p) continue
    const cur = best.get(p.series)
    if (!cur || compareVersion(p.version, cur) > 0) best.set(p.series, p.version)
  }
  const oldIds = new Set<string>()
  const kept: ModelInfo[] = []
  for (const m of models) {
    const p = parseModelSeries(kind, m.id)
    const isLatest = !p || compareVersion(p.version, best.get(p.series) ?? []) === 0
    if (isLatest) kept.push(m)
    else if (modelMatches(m.id, selected)) {
      kept.push(m)
      oldIds.add(m.id)
    }
  }
  return { models: kept, oldIds }
}

export function ensureCurrentModel(
  models: ModelInfo[],
  model: string | null | undefined,
  kind?: BotKind,
): { models: ModelInfo[]; missingIds: ReadonlySet<string> } {
  if (!model) {
    return { models, missingIds: new Set() }
  }
  if (kind === 'agy' && !agyModelAllowed(model)) {
    return { models, missingIds: new Set() }
  }
  const exists = models.some((m) => m.id === model || modelMatches(m.id, model))
  if (exists) {
    return { models, missingIds: new Set() }
  }
  const defaultModel = models.find((m) => m.is_default) ?? models[0]
  const defaultEfforts =
    kind === 'agy'
      ? []
      : defaultModel?.efforts?.length
        ? defaultModel.efforts
        : kind === 'openai-api'
          ? [...openAiApiEffortsFor(model)]
          : kind === 'grok'
            ? [...EFFORT_OPTIONS]
            : kind === 'codex'
              ? [...CODEX_EFFORT_OPTIONS]
              : [...CLAUDE_EFFORT_OPTIONS]
  const missingItem: ModelInfo = {
    id: model,
    display_name: model,
    description: '',
    is_default: false,
    default_effort: defaultModel?.default_effort ?? null,
    efforts: defaultEfforts,
    service_tiers: [],
  }
  return {
    models: [missingItem, ...models],
    missingIds: new Set([model]),
  }
}
