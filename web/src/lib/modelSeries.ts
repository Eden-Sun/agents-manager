import type { BotKind, ModelInfo } from '../api/types'
import {
  agyModelAllowed,
  CLAUDE_EFFORT_OPTIONS,
  CODEX_EFFORT_OPTIONS,
  EFFORT_OPTIONS,
  openAiApiEffortsFor,
} from '../api/types'
import { modelMatches, stripModelContext } from './modelMatches'

/**
 * 模型選單只顯示每個系列的最新版（2026-11-18 使用者：「UI 只要出最新」）。
 *
 * 系列與版號靠各 kind 的命名規則拆出來；拆不出來的模型（例如測試用的 `alpha`、grok）一律照舊顯示。
 * - codex：`gpt-<版號>-<系列>`，例 `gpt-6.2-sol` → 系列 `sol`、版號 `6.2`
 * - claude／claude-api：`claude-<系列>-<版號>`，例 `claude-opus-5-5` → 系列 `opus`、版號 `5.5`；
 *   daemon 也會回別名 `opus`（無版號），視為最舊的同系列版本
 * - agy：`gemini-<版號>-<系列>[-強度]`，例 `gemini-3.8-flash-high` → 系列 `flash`、版號 `3.8`；
 *   強度尾巴不算系列，同一版號的各強度都保留
 *
 * 版號逐段用數字比較（`6.10` > `6.2`）。同一系列取最大版號；版號相同的全部保留。
 */

type Parsed = { series: string; version: number[] }

const toVersion = (s: string) => s.split(/[.-]/).map((x) => Number(x))

export function parseModelSeries(kind: BotKind, id: string): Parsed | null {
  const norm = stripModelContext(id)
  if (kind === 'codex') {
    const m = /^gpt-(\d+(?:\.\d+)*)-([a-z]+)$/.exec(norm)
    return m ? { series: `codex:${m[2]}`, version: toVersion(m[1]) } : null
  }
  if (kind === 'openai-api') {
    // OpenAI API：`gpt-6-luna` → 系列 `luna`，版號 `6`（與 codex 同格式，系列前綴分開）。
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

/** 版號逐段比較：`[6, 10]` > `[6, 2]`；缺的段當 0；空陣列（別名）最小。 */
export function compareVersion(a: number[], b: number[]): number {
  const n = Math.max(a.length, b.length)
  for (let i = 0; i < n; i++) {
    const d = (a[i] ?? 0) - (b[i] ?? 0)
    if (d !== 0) return d
  }
  return 0
}

/**
 * 只留每個系列的最新版；目前設定的模型若是被隱藏的舊版，仍保留並放進 `oldIds`（UI 標「舊版」），
 * 讓使用者看得到現況、可以改成新版。拆不出系列的模型原樣保留。
 */
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

/**
 * 確保目前設定的模型出現在清單中：
 * 若 API 清單／內建備用清單沒有該模型（且不是 agy 已淘汰的舊版），補上一顆虛擬項目，並在 missingIds 註記（標「不在清單」）。
 */
export function ensureCurrentModel(
  models: ModelInfo[],
  model: string | null | undefined,
  kind?: BotKind,
): { models: ModelInfo[]; missingIds: ReadonlySet<string> } {
  if (!model) {
    return { models, missingIds: new Set() }
  }
  // agy 已停用的舊模型（如 gemini-3.1）：由下方 banner 提示改用 3.8 medium，不補進選單。
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
