
export interface AsyncScreen {
  expanded: boolean
  count: number
}

const SCAN = 40

const COLLAPSED = /^\?\s+(\d+)\s+questions?(?:\s.*)?$/
export const ASYNC_QUESTION_FOOTER = /^\s*enter submit\b.*(?:\^|⌃)\]\s*skip/i
const PAGE_OF = /^\d+ of (\d+)$/

export function parseAsyncScreen(text: string | null | undefined): AsyncScreen | null {
  if (!text) return null
  const all = text.split('\n').map((l) => l.trim())
  const lines = all.slice(Math.max(0, all.length - SCAN))
  for (let i = lines.length - 1; i >= 0; i--) {
    const collapsed = COLLAPSED.exec(lines[i])
    if (collapsed && Number(collapsed[1]) > 0) {
      const next = lines.slice(i + 1).find((l) => l !== '')
      if (next?.startsWith('shift+← to answer') || next?.startsWith('⇧← to answer')) return { expanded: false, count: Number(collapsed[1]) }
    }
    if (ASYNC_QUESTION_FOOTER.test(lines[i])) {
      const page = lines
        .slice(Math.max(0, i - 8), i)
        .reverse()
        .map((l) => PAGE_OF.exec(l))
        .find(Boolean)
      return { expanded: true, count: page ? Number(page[1]) : 1 }
    }
  }
  return null
}

const QUEUE_HEADER = 'Queued follow-up inputs'

const squash = (s: string) => s.replace(/\s+/g, '')

export function asyncPanelText(text: string | null | undefined): string | null {
  if (!text) return null
  const lines = text.split('\n')
  let head = -1
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].trim().replace(/^•\s*/, '') === QUEUE_HEADER) {
      head = i
      break
    }
  }
  if (head < 0) return null
  const tail = lines.slice(head + 1)
  const end = tail.findIndex((l) => l.trim().startsWith('enter submit'))
  return (end < 0 ? tail : tail.slice(0, end)).join('\n')
}

function panelLines(text: string | null | undefined): string[] | null {
  const panel = asyncPanelText(text)
  if (panel === null) return null
  const lines = panel.split('\n').map((l) => l.trim()).filter((l) => l !== '')
  if (lines.length && PAGE_OF.test(lines[0])) lines.shift()
  return lines
}

export function panelBody(text: string | null | undefined, question: string): string[] | null {
  if (parseAsyncScreen(text)?.expanded !== true) return null
  const lines = panelLines(text)
  const want = squash(question)
  if (!lines || !want) return null
  let acc = ''
  for (let k = 0; k < lines.length; k++) {
    acc += squash(lines[k])
    if (acc === want) return lines.slice(k + 1)
    if (acc.length >= want.length) return null
  }
  return null
}

export function panelShows(text: string | null | undefined, question: string): boolean {
  return panelBody(text, question) !== null
}

export function panelInput(text: string | null | undefined, question: string): string | null {
  const body = panelBody(text, question)
  return body === null ? null : squash(body.join(''))
}

export const EMPTY_INPUT = squash('Type your answer')

export const inputEquals = (input: string | null, typed: string): boolean => input !== null && input === squash(typed)
