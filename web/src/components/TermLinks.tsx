import type { ReactNode } from 'react'
import { TermLink } from './TermLink'

/**
 * URL 在終端畫面裡是可點的：點一下複製到剪貼簿（不是開新分頁——這種 URL 多半是登入用的
 * 一次性連結，使用者要的是貼到別處，開了反而跳錯瀏覽器／錯帳號）。
 *
 * 終端會把長 URL 硬折行：一行剛好塞滿 `columns` 欄就接到下一行開頭，中間沒有任何分隔。
 * 畫面上維持原本的折行（不然 `<pre>` 會橫向捲動），但每一段都指向**接回來的完整 URL**，
 * 點哪一段複製到的都是整條。`columns` 不知道時拿最長的一行當寬度。
 */
const URL_CHARS = /[^\s<>"'`）」』]/
const URL_RE = /https?:\/\/[^\s<>"'`）」』]+/g

/** 終端不會窄到這個以下；比這短的「最長一行」只是內容短，不是折行寬度。 */
const MIN_WRAP_WIDTH = 40

type Piece = { text: string; url: string | null }

/** 這一行接得上上一行的 URL 嗎（開頭是連續的網址字元，不是空白也不是別的符號）。 */
function continuesUrl(line: string | undefined): boolean {
  return line !== undefined && line.length > 0 && !/^\s/.test(line) && URL_CHARS.test(line[0])
}

/** 整行都是網址字元、一個空白都沒有——軟折行中段的長相。 */
function isFullWrap(line: string | undefined, width: number): boolean {
  return line !== undefined && line.length === width && !/\s/.test(line) && continuesUrl(line)
}

/**
 * 第 `i` 行的 URL 剛好跑到行尾——這是終端把它折下去了，還是它本來就在這裡結束？
 *
 * 不能只看「這行是不是畫面上最長的一行」：claude 的登入畫面有一條 185 欄的框線，URL 卻是
 * 在 78 欄折的，拿最長行當寬度時每一段 URL 都「沒塞滿」，於是只有第一行被 linkify、複製到的
 * 是腰斬的網址（實測 2026-09-08）。所以改成三條各自獨立的證據，中一條就接：
 *
 *   (a) 下一行是「剛好同寬、整行沒有空白」的續行——只有軟折行會長這樣，跟畫面上其他東西
 *       多寬無關。折成三行以上的長 URL 都吃這條。
 *   (b) 這行的長度剛好等於終端的 `columns`：典型的硬折行，就算只折一次也算數。
 *   (c) 這行剛好是畫面上最長的一行（舊行為，`observed`）：折一次、又不知道 columns 時的後路。
 *
 * 都不中就不接——寧可漏接也不要把真正換行的相鄰兩行黏成一條假網址。
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

/** 逐行拆成「純文字」與「URL 片段（帶完整 URL）」。導出是為了測試。 */
export function termPieces(text: string, columns?: number | null): Piece[][] {
  const lines = text.split('\n')
  const longest = Math.max(0, ...lines.map((l) => l.length))
  const rows: Piece[][] = []
  // 上一行的 URL 跑到行尾且判定為折行 → 這一行開頭的連續非空白是它的延續。
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
        // 整行都是這條 URL 而且跟上一行等寬 → 還會再折下去；否則 URL 就在這一行結束。
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

/** 終端文字 → 內含可點 URL 的節點。沒有 URL 時回原字串，`<pre>` 照常顯示。 */
export function linkifyTerm(text: string, columns?: number | null): ReactNode {
  if (!/https?:\/\//.test(text)) return text
  const out: ReactNode[] = []
  termPieces(text, columns).forEach((row, i) => {
    if (i > 0) out.push('\n')
    for (const p of row) {
      if (p.url) out.push(<TermLink key={`${i}:${out.length}`} url={p.url} text={p.text} />)
      else out.push(p.text)
    }
  })
  return out
}
