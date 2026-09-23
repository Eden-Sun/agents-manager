/**
 * claude 停在 `AskUserQuestion` 時，從 transcript 讀出來的題目（daemon `GET /api/bots/{id}/pending-question`）。
 *
 * 2026-09-23 使用者：「為什麼看不見題目」——pane 只有 14 行，claude 把自己的選單裁掉，題目那行根本沒畫出來，
 * 選項也只剩捲動中的一段（`↓ 3.`、跳到 `5.`），網頁從畫面怎麼 parse 都拿不到。transcript 裡有完整的題目。
 */
export interface PendingOption {
  label: string
  description: string
}

export interface PendingQuestion {
  header: string
  question: string
  multiSelect: boolean
  options: PendingOption[]
}

const str = (v: unknown) => (typeof v === 'string' ? v : '')

/** daemon 的 `questions`；壞的、空的丟掉，全部都沒有回空陣列。 */
export function toPendingQuestions(raw: unknown): PendingQuestion[] {
  if (!Array.isArray(raw)) return []
  const out: PendingQuestion[] = []
  for (const q of raw) {
    if (!q || typeof q !== 'object') continue
    const r = q as Record<string, unknown>
    const question = str(r.question).trim()
    if (!question) continue
    const options = Array.isArray(r.options)
      ? r.options
          .map((o) => (o && typeof o === 'object' ? (o as Record<string, unknown>) : {}))
          .map((o) => ({ label: str(o.label).trim(), description: str(o.description).trim() }))
          .filter((o) => o.label)
      : []
    out.push({ header: str(r.header).trim(), question, multiSelect: r.multiSelect === true, options })
  }
  return out
}

/**
 * 畫面上已經看得到這題就不要再疊一張卡：畫面認出來的問句（`menu.question`，折行已接回）有包含這題的前 12 個字
 * 就算看得到。比對時拿掉空白——畫面折行會插空白。
 */
export function questionVisibleOnScreen(pending: PendingQuestion[], screenQuestion: string | null | undefined): boolean {
  if (!screenQuestion || pending.length === 0) return false
  const squash = (s: string) => s.replace(/\s+/g, '')
  const shown = squash(screenQuestion)
  return pending.every((q) => shown.includes(squash(q.question).slice(0, 12)))
}
