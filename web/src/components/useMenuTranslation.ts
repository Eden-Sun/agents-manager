import type { PendingQuestion } from '../lib/pendingQuestion'
import { usePendingQuestionTranslation } from './usePendingQuestionTranslation'

const MAX_PAGES = 8

export interface MenuPageText {
  header: string
  question: string | null
  multi: boolean
  choices: { title: string; detail: string }[]
}

export interface MenuPageDisplay {
  header: string | null
  question: string | null
  titles: (string | null)[]
  details: (string | null)[]
}

function toQuestion(p: MenuPageText): PendingQuestion {
  return {
    header: p.header,
    question: p.question ?? '',
    multiSelect: p.multi,
    options: p.choices.map((c) => ({ label: c.title, description: c.detail })),
    jev: null,
  }
}

export function useMenuTranslation(pages: MenuPageText[]): {
  button: React.ReactNode
  page: (k: number) => MenuPageDisplay | null
} {
  const usable = pages.length > 0 && pages.length <= MAX_PAGES ? pages : []
  const { button, display } = usePendingQuestionTranslation(usable.map(toQuestion))
  const page = (k: number): MenuPageDisplay | null => {
    const d = display[k]
    const p = usable[k]
    if (!d || !p) return null
    return {
      header: d.header || null,
      question: d.question || null,
      titles: p.choices.map((_, j) => d.options[j]?.label || null),
      details: p.choices.map((_, j) => d.options[j]?.description || null),
    }
  }
  return { button, page }
}
