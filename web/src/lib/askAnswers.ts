import type { Message } from '../api/types'

/**
 * claude 用 `AskUserQuestion` 問使用者、使用者答完之後，daemon 把題目與答案記成一則系統訊息
 * （`daemon/src/ask_answers.rs`）：`messages.source` 有 CHECK，沒有新的來源值可用，
 * 所以靠 `content` 的 JSON `type` 認。這則**不是**使用者打的 prompt，也不是 claude 的回覆。
 */
export interface AskItem {
  header: string | null
  question: string
  /** null＝沒有回答（整組被取消，或多題裡沒答這一題）。 */
  answer: string | null
  notes: string | null
}

export interface AskAnswers {
  toolUseId: string
  /** 至少有一題答了；全部沒答（Cancel／Esc）是 false。 */
  answered: boolean
  items: AskItem[]
}

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null)

/** 認不出來（不是這種訊息、JSON 壞了、沒有題目）就是 null——呼叫端照一般系統訊息畫。 */
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

export const NOT_ANSWERED = '沒有回答'

/** 點一下複製用的純文字。 */
export function askAnswersText(a: AskAnswers): string {
  return a.items
    .map((i, n) => `${n + 1}. ${i.header ? `[${i.header}] ` : ''}${i.question}\n   → ${i.answer ?? NOT_ANSWERED}${i.notes ? `（備註：${i.notes}）` : ''}`)
    .join('\n')
}
