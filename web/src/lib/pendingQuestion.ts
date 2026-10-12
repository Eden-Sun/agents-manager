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

/**
 * Jev 對這一題的建議選項（daemon `jev_options`，issue #1475，SPEC §4.3c 第五個場景）：只提示，不代答。
 * `option` 是 `options[]` 裡的位置（已過濾掉沒有 label 的選項），`label` 是那一項的字，`score` 是 Jev 給的 0–1 分數。
 */
export interface JevPick {
  option: number
  label: string
  score: number
}

export interface PendingQuestion {
  header: string
  question: string
  multiSelect: boolean
  options: PendingOption[]
  /** 沒有建議（Jev 沒開、沒問、失敗、分數太接近、多選題）就沒有這個欄位或 `null`——畫面什麼都不顯示。 */
  jev?: JevPick | null
}

const str = (v: unknown) => (typeof v === 'string' ? v : '')

const isRec = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v)

/**
 * daemon `jev_options.questions`：`{index, scores, suggested}` 的陣列，`index` 是 `questions[]` 的原位置。
 * 只信 daemon 已經決定的 `suggested`（門檻與差距在 daemon 判，網頁不重算）；壞的一律當沒有。
 */
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

/**
 * daemon 的 `questions`；壞的、空的丟掉，全部都沒有回空陣列。
 * `jevRaw`（回應的 `jev_options`）有就把建議掛到對應那一題：位置用原 `questions[]` 的，
 * 所以前面有題目被丟掉也不會掛錯；選項數對不上、多選題、分數不在 0–1 一律不掛。
 */
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

/** `GET /api/bots/{id}/pending-question` 的整份回應 `{questions, jev_options}`（舊 daemon 沒有 `jev_options`）。 */
export function toPendingQuestionsFromResponse(resp: unknown): PendingQuestion[] {
  return isRec(resp) ? toPendingQuestions(resp.questions, resp.jev_options) : []
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
  /** Jev 對這一題的建議（`BlockedChoices` 在對得上的那一列標「Jev 建議」）。 */
  jev: JevPick | null
}

export function pendingAtOnScreen(
  pending: PendingQuestion[],
  menu: Parameters<typeof pendingIndexOnScreen>[1],
): PendingAt | null {
  const at = pendingIndexOnScreen(pending, menu)
  return at < 0 ? null : { at, total: pending.length, question: pending[at].question, header: pending[at].header, jev: pending[at].jev ?? null }
}

/**
 * 畫面上選單那一列的標題是不是 Jev 建議的那個選項：去掉空白後相等；列太寬被終端截成 `…` 結尾時比前綴。
 * 對不上就不標——寧可少標，不要標到別列。
 */
export function jevMatchesTitle(label: string, title: string): boolean {
  const squash = (s: string) => s.replace(/\s+/g, '')
  const a = squash(label)
  const b = squash(title)
  if (!a || !b) return false
  if (a === b) return true
  const cut = b.replace(/(…|\.\.\.)$/, '')
  return cut !== b && cut.length >= 4 && a.startsWith(cut)
}
