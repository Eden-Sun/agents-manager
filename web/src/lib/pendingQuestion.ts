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

/**
 * 畫面上這份選單是原題裡的第幾題（issue #559）；對不上回 -1。
 *
 * 先比問句（同 `questionVisibleOnScreen` 的前 12 個字）；畫面沒畫出問句（pane 太矮）時改比選項：原題的每個選項
 * 都能在畫面的選項標題裡找到才算。多題的標題可能一樣（例如兩題都有「先不關」），所以要**全部**對上、而且只認一題，
 * 兩題都對得上就不猜。
 */
export function pendingIndexOnScreen(
  pending: PendingQuestion[],
  menu: { question: string | null; choices: { title: string }[] } | null | undefined,
): number {
  if (!menu || pending.length === 0) return -1
  const squash = (s: string) => s.replace(/\s+/g, '')
  if (menu.question) {
    const shown = squash(menu.question)
    const byQuestion = pending.findIndex((q) => shown.includes(squash(q.question).slice(0, 12)))
    if (byQuestion >= 0) return byQuestion
  }
  const titles = menu.choices.map((c) => squash(c.title))
  const hits = pending
    .map((q, i) => ({ i, ok: q.options.length > 0 && q.options.every((o) => titles.includes(squash(o.label))) }))
    .filter((h) => h.ok)
  return hits.length === 1 ? hits[0].i : -1
}

/** 畫面上這一題在原題裡的位置與題目（`BlockedChoices` 用來補沒畫出來的題目、第幾題）。 */
export interface PendingAt {
  at: number
  total: number
  question: string
  header: string
}

export function pendingAtOnScreen(
  pending: PendingQuestion[],
  menu: Parameters<typeof pendingIndexOnScreen>[1],
): PendingAt | null {
  const at = pendingIndexOnScreen(pending, menu)
  return at < 0 ? null : { at, total: pending.length, question: pending[at].question, header: pending[at].header }
}
