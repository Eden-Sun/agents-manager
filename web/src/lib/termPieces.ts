/**
 * 終端文字裡的 URL 怎麼切（純函式，沒有 JSX）。
 *
 * 單獨一個檔是為了測試：`node --test --experimental-strip-types` 跑不了 `.tsx`，
 * 規則本身又是這裡最容易錯的一段（折行接回、標點不算網址）。
 */
export type Piece = { text: string; url: string | null }

/**
 * 終端裡的 URL 點一下複製（多半是一次性登入連結，開新分頁反而跳錯瀏覽器／帳號）。
 * 長 URL 被硬折行時保留畫面折行，但每一段都指向接回來的完整 URL。
 */
const URL_CHARS = /[^\s<>"'`）」』]/
const URL_RE = /https?:\/\/[^\s<>"'`）」』]+/g

/** 終端不會窄到這個以下；比這短的「最長一行」只是內容短，不是折行寬度。 */
const MIN_WRAP_WIDTH = 40

/** 這一行接得上上一行的 URL 嗎（開頭是連續的網址字元，不是空白也不是別的符號）。 */
function continuesUrl(line: string | undefined): boolean {
  return line !== undefined && line.length > 0 && !/^\s/.test(line) && URL_CHARS.test(line[0])
}

/** 整行都是網址字元、一個空白都沒有——軟折行中段的長相。 */
function isFullWrap(line: string | undefined, width: number): boolean {
  return line !== undefined && line.length === width && !/\s/.test(line) && continuesUrl(line)
}

/**
 * 第 `i` 行的 URL 跑到行尾，是被折下去還是本來就結束？不能只看最長行：claude 登入畫面有 185 欄框線、URL 卻在 78 欄折（實測 2026-09-08）。
 * 三條證據中一條就接：(a) 下一行同寬且無空白 (b) 行長等於 `columns` (c) 是畫面最長行（不知 columns 的後路）。都不中寧可漏接。
 */
function wrapsToNextLine(lines: string[], i: number, columns: number | null | undefined, longest: number): boolean {
  const width = lines[i].length
  if (width === 0 || !continuesUrl(lines[i + 1])) return false
  // 比終端還寬的一行不可能是折行的結果（多半是快照裡的裝飾線）。
  if (columns && columns > 0 && width > columns) return false
  if (isFullWrap(lines[i + 1], width)) return true
  if (columns && columns > 0 && width === columns) return true
  return width >= MIN_WRAP_WIDTH && width === longest
}

/** 逐行拆成「純文字」與「URL 片段（帶完整 URL）」。 */
export function termPieces(text: string, columns?: number | null): Piece[][] {
  const lines = text.split('\n')
  const longest = Math.max(0, ...lines.map((l) => l.length))
  const rows: Piece[][] = []
  let carry: { url: string; frags: Piece[]; width: number } | null = null
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]
    const row: Piece[] = []
    let from = 0
    if (carry) {
      const m = /^[^\s]+/.exec(line)
      if (m && URL_CHARS.test(m[0][0])) {
        const piece: Piece = { text: m[0], url: null }
        carry.frags.push(piece)
        carry.url += m[0]
        row.push(piece)
        from = m[0].length
        if (from !== line.length || line.length !== carry.width) {
          for (const f of carry.frags) f.url = trimPunct(carry.url)
          carry = null
        }
      } else {
        for (const f of carry.frags) f.url = trimPunct(carry.url)
        carry = null
      }
    }
    if (from < line.length) {
      const rest = line.slice(from)
      let last = 0
      for (const m of rest.matchAll(URL_RE)) {
        const at = m.index ?? 0
        if (at > last) row.push({ text: rest.slice(last, at), url: null })
        const endsLine = at + m[0].length === rest.length && wrapsToNextLine(lines, i, columns, longest)
        const piece: Piece = { text: m[0], url: endsLine ? null : trimPunct(m[0]) }
        row.push(piece)
        if (endsLine) carry = { url: m[0], frags: [piece], width: line.length }
        last = at + m[0].length
      }
      if (last < rest.length) row.push({ text: rest.slice(last), url: null })
    }
    rows.push(row)
  }
  if (carry) for (const f of carry.frags) f.url = trimPunct(carry.url)
  return rows
}

/** 句尾標點不算網址的一部分。 */
function trimPunct(url: string): string {
  let u = url
  while (/[.,;:!?]$/.test(u)) u = u.slice(0, -1)
  return u
}
