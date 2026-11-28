import type { JudgeShadowRow } from '../api/judge'
import { msg, t } from './i18n'

export type JudgeScene = 'limit_hit' | 'stuck_queued' | 'same_work' | 'report_evidence' | 'ask_option' | 'push_worth'

export const JUDGE_SCENES: readonly { id: JudgeScene; name: string; when: string }[] = [
  {
    id: 'limit_hit',
    name: msg('撞限第二意見'),
    when: msg('codex／grok 撞到用量上限，畫面比對判定是新的撞限時，問一題：命中的那一行是介面畫的，還是 bot 印出來的內容。'),
  },
  {
    id: 'stuck_queued',
    name: msg('卡在不認識的畫面'),
    when: msg('有 prompt 排在 queued 超過 180 秒、bot 的 run 是 running／idle、輸入列空著時，問畫面底部是不是有等人選的框。'),
  },
  {
    id: 'same_work',
    name: msg('撞題提示'),
    when: msg('派工或開票時，同專案在跑的工作裡有沒有同一件事，問一次。只提示，不擋派工、不改認領。'),
  },
  {
    id: 'report_evidence',
    name: msg('完成回報的證據'),
    when: msg('交辦從在途收成，且回報是 hook 的完成回覆（不含從終端機刮下來的）時，問回報有沒有宣稱已驗證，以及要不要收件者動手。'),
  },
  {
    id: 'ask_option',
    name: msg('選項建議'),
    when: msg('claude 用 AskUserQuestion 問單選題時，問使用者最可能選哪一項；最高分 ≥0.7 且比第二名高 ≥0.15 才在選項旁標「Jev 建議」。只提示，不代答。'),
  },
  {
    id: 'push_worth',
    name: msg('推播過濾'),
    when: msg('主力 bot 完成回覆即將推播給使用者時，問這則回覆是否值得通知；機率 <0.35（例如僅回「已分配任務」）才跳過推播。'),
  },
]

export const JUDGE_WINDOWS = [
  { id: 'day', label: msg('近 24 小時'), ms: 24 * 3600_000 },
  { id: 'week', label: msg('近 7 天'), ms: 7 * 24 * 3600_000 },
  { id: 'all', label: msg('全部'), ms: Infinity },
] as const

export type JudgeWindow = (typeof JUDGE_WINDOWS)[number]['id']

export function sameWorkStale(row: JudgeShadowRow): string | null {
  if (row.regex_verdict !== 'same_work' || !row.matched_line) return null
  try {
    const v: unknown = JSON.parse(row.matched_line)
    const s = typeof v === 'object' && v !== null ? (v as Record<string, unknown>).stale : undefined
    return typeof s === 'string' && s !== '' ? s : null
  } catch {
    return null
  }
}

const isError = (row: JudgeShadowRow) => row.error !== null && row.error !== ''

export function askOptionQuestions(row: JudgeShadowRow): { index: number; scores: number[]; suggested: number | null }[] | null {
  if (row.regex_verdict !== 'ask_option' || !row.matched_line || isError(row)) return null
  try {
    const v: unknown = JSON.parse(row.matched_line)
    const qs = typeof v === 'object' && v !== null ? (v as Record<string, unknown>).questions : undefined
    if (!Array.isArray(qs)) return null
    const out: { index: number; scores: number[]; suggested: number | null }[] = []
    for (const q of qs) {
      if (typeof q !== 'object' || q === null) return null
      const r = q as Record<string, unknown>
      if (!Array.isArray(r.scores) || !r.scores.every((n) => typeof n === 'number' && Number.isFinite(n))) return null
      out.push({
        index: typeof r.index === 'number' ? r.index : out.length,
        scores: r.scores as number[],
        suggested: typeof r.suggested === 'number' && Number.isInteger(r.suggested) ? r.suggested : null,
      })
    }
    return out
  } catch {
    return null
  }
}

export function overThreshold(row: JudgeShadowRow): boolean | null {
  switch (row.regex_verdict) {
    case 'stuck_queued':
      return row.jev_is_live_ui !== null && row.jev_is_live_ui >= 0.7
    case 'same_work':
      return sameWorkStale(row) === null && row.jev_same_work !== null && row.jev_same_work >= 0.5
    case 'report_evidence':
      return (row.claims_verified !== null && row.claims_verified < 0.8) || (row.asks_parent_action !== null && row.asks_parent_action >= 0.8)
    case 'ask_option':
      return askOptionQuestions(row)?.some((q) => q.suggested !== null) === true
    case 'push_worth':
      return row.jev_is_live_ui !== null && row.jev_is_live_ui < 0.35
    default:
      return null
  }
}

export interface SceneWindowCount {
  total: number
  over: number | null
  errors: number
}

export interface SceneStat {
  scene: JudgeScene
  windows: Record<JudgeWindow, SceneWindowCount>
  last: string | null
}

function inWindow(at: string, ms: number, now: number): boolean {
  const t = Date.parse(at)
  return Number.isNaN(t) ? false : now - t <= ms
}

export function sceneStats(rows: readonly JudgeShadowRow[], now: number = Date.now()): SceneStat[] {
  return JUDGE_SCENES.map(({ id }) => {
    const mine = rows.filter((r) => r.regex_verdict === id)
    const windows = {} as Record<JudgeWindow, SceneWindowCount>
    for (const w of JUDGE_WINDOWS) {
      const picked = w.id === 'all' ? mine : mine.filter((r) => inWindow(r.at, w.ms, now))
      const over = id === 'limit_hit' ? null : picked.filter((r) => overThreshold(r) === true).length
      windows[w.id] = { total: picked.length, over, errors: picked.filter(isError).length }
    }
    let last: string | null = null
    let lastT = Number.NaN
    for (const r of mine) {
      const t = Date.parse(r.at)
      if (!Number.isNaN(t) && (Number.isNaN(lastT) || t > lastT)) {
        lastT = t
        last = r.at
      }
    }
    return { scene: id, windows, last }
  })
}

export function recentRows(rows: readonly JudgeShadowRow[], limit = 20): JudgeShadowRow[] {
  const key = (r: JudgeShadowRow) => {
    const t = Date.parse(r.at)
    return Number.isNaN(t) ? Number.NEGATIVE_INFINITY : t
  }
  return [...rows].sort((a, b) => key(b) - key(a)).slice(0, limit)
}

const fmt = (n: number | null) => (n === null ? '—' : n.toFixed(2))

export function probabilityText(row: JudgeShadowRow): string {
  switch (row.regex_verdict) {
    case 'stuck_queued':
    case 'limit_hit':
    case 'push_worth':
      return fmt(row.jev_is_live_ui)
    case 'same_work':
      return fmt(row.jev_same_work)
    case 'report_evidence':
      return t('驗 {verified}／要 {ask}', { verified: fmt(row.claims_verified), ask: fmt(row.asks_parent_action) })
    case 'ask_option': {
      const qs = askOptionQuestions(row)
      if (!qs || qs.length === 0) return '—'
      const picked = qs.find((q) => q.suggested !== null)
      if (picked) return t('建議 {score}', { score: fmt(picked.scores[picked.suggested as number] ?? null) })
      const all = qs.flatMap((q) => q.scores)
      return all.length ? t('無建議（最高 {score}）', { score: fmt(Math.max(...all)) }) : '—'
    }
    default:
      return '—'
  }
}

export function botLabel(id: string | null, bots: readonly { id: string; name: string }[]): string {
  if (id === null) return '—'
  const issue = /^issue:(\d+)$/.exec(id)
  if (issue) return t('#{n}（開票）', { n: issue[1] })
  return bots.find((b) => b.id === id)?.name ?? id.slice(0, 6)
}
