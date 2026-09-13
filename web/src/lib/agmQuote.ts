/**
 * user 訊息其實是誰的話（2026-09-12 使用者：「算在 AGM 訊息不算在 user」）。補 `relay_from` 為空的
 * 情形：使用者貼 AGM 裁示、或 herdr prompt 回音沒對上 relay。只認第一行開頭，寧可漏認；`null`＝使用者自己。
 */

const AGM_COLON_RE = /^\s*(AGM|AG ?Man)\b[^\n]{0,24}?[：:]/
const AGM_BRACKET_RE = /^\s*\[\s*(AGM|AG ?Man)\b[^\]\n]{0,60}\]/
/** `[來自 c1-主要功能 / agents-manager-qn0ssg]`：抓 bot 名。 */
const FROM_BRACKET_RE = /^\s*\[\s*來自\s*([^/\]\n]{1,40}?)\s*(?:\/[^\]\n]{0,60})?\]/

export function quotedFrom(content: string): string | null {
  const first = content.trimStart().split('\n', 1)[0] ?? ''
  const named = FROM_BRACKET_RE.exec(first)
  if (named) return named[1]
  if (AGM_COLON_RE.test(first) || AGM_BRACKET_RE.test(first)) return 'AGM'
  return null
}

/** 舊名，保留給只想知道「是不是總管的話」的呼叫端。 */
export function isAgmQuote(content: string): boolean {
  return quotedFrom(content) === 'AGM'
}
