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

/** 逐行拆成「純文字」與「URL 片段（帶完整 URL）」。導出是為了測試。 */
export function termPieces(text: string, columns?: number | null): Piece[][] {
  const lines = text.split('\n')
  // 折行寬度不能只信 `columns`：pane 現在 185 欄，但那段輸出可能是在較窄時印的（實測
  // 2026-09-08：185 欄的 pane 裡 URL 每 78 字就折），所以拿「畫面上最長的一行」跟 columns
  // 取小的當寬度——硬折行的 URL 一定會把那個寬度塞滿。
  // 太短就不算折行（畫面上只有一條短 URL 加提示字元時，最長那行不是寬度）。
  const longest = Math.max(0, ...lines.map((l) => l.length))
  const observed = longest >= MIN_WRAP_WIDTH ? longest : 0
  const width = columns && columns > 0 ? Math.min(columns, observed || columns) : observed
  const rows: Piece[][] = []
  // 上一行的 URL 跑到行尾且那行塞滿了 → 這一行開頭的連續非空白是它的延續。
  let carry: { url: string; frags: Piece[] } | null = null
  for (const line of lines) {
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
        if (line.length < width) {
          for (const f of carry.frags) f.url = trimPunct(carry.url)
          carry = null
        }
      } else {
        for (const f of carry.frags) f.url = trimPunct(carry.url)
        carry = null
      }
    }
    if (carry === null || from < line.length) {
      const rest = line.slice(from)
      let last = 0
      for (const m of rest.matchAll(URL_RE)) {
        const at = m.index ?? 0
        if (at > last) row.push({ text: rest.slice(last, at), url: null })
        const endsLine = at + m[0].length === rest.length && line.length >= width && width > 0
        const piece: Piece = { text: m[0], url: endsLine ? null : trimPunct(m[0]) }
        row.push(piece)
        if (endsLine) carry = { url: m[0], frags: [piece] }
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
