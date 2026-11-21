import type { Message } from '../api/types'
import { msg, t, tr } from './i18n'

export interface AskItem {
  header: string | null
  question: string
  answer: string | null
  notes: string | null
}

export interface AskAnswers {
  toolUseId: string
  answered: boolean
  items: AskItem[]
}

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null)

export function parseAskAnswers(msg: Pick<Message, 'role' | 'id' | 'content'>): AskAnswers | null {
  if (msg.role !== 'system' || !msg.id.startsWith('ask:') || !msg.content.startsWith('{')) return null
  let raw: unknown
  try {
    raw = JSON.parse(msg.content)
  } catch {
    return null
  }
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  if (o.type !== 'ask_answers' || !Array.isArray(o.items)) return null
  const items: AskItem[] = []
  for (const it of o.items) {
    if (typeof it !== 'object' || it === null) continue
    const i = it as Record<string, unknown>
    const question = str(i.question)
    if (!question) continue
    items.push({ header: str(i.header), question, answer: str(i.answer), notes: str(i.notes) })
  }
  if (!items.length) return null
  return { toolUseId: str(o.tool_use_id) ?? '', answered: items.some((i) => i.answer !== null), items }
}

export const NOT_ANSWERED = msg('沒有回答')

export function askAnswersText(a: AskAnswers): string {
  return a.items
    .map((i, n) => `${n + 1}. ${i.header ? `[${i.header}] ` : ''}${i.question}\n   → ${i.answer ?? tr(NOT_ANSWERED)}${i.notes ? ` (${t('備註：{notes}', { notes: i.notes })})` : ''}`)
    .join('\n')
}
