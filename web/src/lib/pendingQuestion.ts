import { JEV_ADVISORY_NOTE, JEV_FULL_NAME } from './jevLabel'
import { msg, t, tr } from './i18n'

export interface PendingOption {
  label: string
  description: string
}

export interface JevPick {
  option: number
  label: string
  score: number
}

export const JEV_ASK_NOTE = msg('Jev（TypeSafe 型別判斷）的建議，只是建議，不會自動動作，不會替你作答；請自己選。')

export function jevAskNote(): string {
  return t('{name}的建議，{note}，不會替你作答；請自己選。', {
    name: tr(JEV_FULL_NAME),
    note: tr(JEV_ADVISORY_NOTE),
  })
}

export interface PendingQuestion {
  header: string
  question: string
  multiSelect: boolean
  options: PendingOption[]
  jev?: JevPick | null
}

const str = (v: unknown) => (typeof v === 'string' ? v : '')

const isRec = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v)

function jevByQuestion(jevRaw: unknown): Map<number, { suggested: number; scores: number[] }> {
  const out = new Map<number, { suggested: number; scores: number[] }>()
  if (!isRec(jevRaw) || !Array.isArray(jevRaw.questions)) return out
  for (const q of jevRaw.questions) {
    if (!isRec(q) || !Number.isInteger(q.index) || !Number.isInteger(q.suggested) || !Array.isArray(q.scores)) continue
    const scores = q.scores.filter((n): n is number => typeof n === 'number' && Number.isFinite(n))
    if (scores.length !== q.scores.length) continue
    out.set(q.index as number, { suggested: q.suggested as number, scores })
  }
  return out
}

export function toPendingQuestions(raw: unknown, jevRaw?: unknown): PendingQuestion[] {
  if (!Array.isArray(raw)) return []
  const picks = jevByQuestion(jevRaw)
  const out: PendingQuestion[] = []
  raw.forEach((q, qi) => {
    if (!q || typeof q !== 'object') return
    const r = q as Record<string, unknown>
    const question = str(r.question).trim()
    if (!question) return
    const rawOptions = Array.isArray(r.options) ? r.options : []
    const kept = rawOptions
      .map((o, rawIndex) => {
        const rec = o && typeof o === 'object' ? (o as Record<string, unknown>) : {}
        return { label: str(rec.label).trim(), description: str(rec.description).trim(), rawIndex }
      })
      .filter((o) => o.label)
    const multiSelect = r.multiSelect === true
    let jev: JevPick | null = null
    const pick = picks.get(qi)
    if (pick && !multiSelect && pick.scores.length === rawOptions.length) {
      const score = pick.scores[pick.suggested]
      const option = kept.findIndex((o) => o.rawIndex === pick.suggested)
      if (option >= 0 && score !== undefined && score >= 0 && score <= 1) jev = { option, label: kept[option].label, score }
    }
    out.push({
      header: str(r.header).trim(),
      question,
      multiSelect,
      options: kept.map((o) => ({ label: o.label, description: o.description })),
      jev,
    })
  })
  return out
}

export function toPendingQuestionsFromResponse(resp: unknown): PendingQuestion[] {
  return isRec(resp) ? toPendingQuestions(resp.questions, resp.jev_options) : []
}

export function questionVisibleOnScreen(pending: PendingQuestion[], screenQuestion: string | null | undefined): boolean {
  if (!screenQuestion || pending.length === 0) return false
  const squash = (s: string) => s.replace(/\s+/g, '')
  const shown = squash(screenQuestion)
  return pending.every((q) => shown.includes(squash(q.question).slice(0, 12)))
}

export function pendingIndexOnScreen(
  pending: PendingQuestion[],
  menu: { question: string | null; choices: { title: string }[] } | null | undefined,
): number {
  if (!menu || pending.length === 0) return -1
  const squash = (s: string) => s.replace(/\s+/g, '')
  if (menu.question) {
    const shown = squash(menu.question)
    const byQuestion = pending.flatMap((q, i) => (shown.includes(squash(q.question).slice(0, 12)) ? [i] : []))
    if (byQuestion.length === 1) return byQuestion[0]
  }
  const titles = menu.choices.map((c) => squash(c.title))
  const hits = pending
    .map((q, i) => ({ i, ok: q.options.length > 0 && q.options.every((o) => titles.includes(squash(o.label))) }))
    .filter((h) => h.ok)
  return hits.length === 1 ? hits[0].i : -1
}

export interface PendingAt {
  at: number
  total: number
  question: string
  header: string
  jev: JevPick | null
}

export function pendingAtOnScreen(
  pending: PendingQuestion[],
  menu: Parameters<typeof pendingIndexOnScreen>[1],
): PendingAt | null {
  const at = pendingIndexOnScreen(pending, menu)
  return at < 0 ? null : { at, total: pending.length, question: pending[at].question, header: pending[at].header, jev: pending[at].jev ?? null }
}

export function jevMatchesTitle(label: string, title: string): boolean {
  const squash = (s: string) => s.replace(/\s+/g, '')
  const a = squash(label)
  const b = squash(title)
  if (!a || !b) return false
  if (a === b) return true
  const cut = b.replace(/(…|\.\.\.)$/, '')
  return cut !== b && cut.length >= 4 && a.startsWith(cut)
}
