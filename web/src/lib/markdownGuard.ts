/**
 * Markdown 轉換的成本上限。bot 的輸出不可信（可能被 prompt injection）：
 * - remark 對某些輸入是平方時間（實測 `*a` 重複 2 萬次＝4 萬字，單執行緒卡 22 秒；`~a` 45 秒），
 *   所以超過這個字數預設純文字，使用者按了才展開（那是使用者自己的選擇）。
 * - 巢狀太深（`> > > …`、縮排很深的清單）會讓轉換遞迴爆 stack，那條由 `SafeMarkdown` 的 error boundary 接住。
 */
export const MARKDOWN_MAX_CHARS = 20_000

/** 引用／清單的巢狀上限：正常文件不會超過十幾層；`> ` 重複 6000 次實測在 DOM 裡跑 87 秒（更深會遞迴爆 stack）。 */
export const MARKDOWN_MAX_QUOTE_DEPTH = 20
export const MARKDOWN_MAX_INDENT = 80

export function markdownTooLong(text: string): boolean {
  return text.length > MARKDOWN_MAX_CHARS
}

/** 單趟掃過去（O(n)）：任何一行的引用層數（`>` 的個數，中間可夾空白）或行首縮排超過上限就算太深。 */
export function markdownTooDeep(text: string): boolean {
  let i = 0
  const n = text.length
  while (i < n) {
    let indent = 0
    while (i < n && (text[i] === ' ' || text[i] === '\t')) {
      indent += text[i] === '\t' ? 4 : 1
      i++
    }
    if (indent > MARKDOWN_MAX_INDENT && i < n && text[i] !== '\n') return true // 只有空白的行不算
    let quotes = 0
    while (i < n && (text[i] === '>' || text[i] === ' ')) {
      if (text[i] === '>' && ++quotes > MARKDOWN_MAX_QUOTE_DEPTH) return true
      i++
    }
    while (i < n && text[i] !== '\n') i++
    i++
  }
  return false
}
