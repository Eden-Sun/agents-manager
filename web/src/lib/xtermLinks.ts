/**
 * 終端畫面上的網址（主機 shell 的 xterm，`HostShellXterm`）。herdr observe 送的是一列一列畫好的畫面：
 * shell 把長網址折到下一列時，xterm 不知道那是同一行（沒有 isWrapped），內建的 web-links addon 只抓得到第一列。
 * 這裡把「寫滿到最後一欄」的列跟下一列接起來再找網址，回傳每段網址落在哪些格子。
 */

/** 一列的每一格：寬字的第二格是 `''`（不佔字），空格是 `' '`。 */
export type CellRow = string[]

export interface CellPos {
  /** 0 起算的欄。 */
  x: number
  /** 0 起算的列（buffer 列）。 */
  y: number
}

export interface FoundLink {
  url: string
  start: CellPos
  /** 含這一格。 */
  end: CellPos
}

/** 往上下最多接幾列（避免整個畫面都是滿列時掃太遠）。 */
const MAX_SPAN = 12
const URL_RE = /https?:\/\/[^\s"'<>`]+/g

function full(row: CellRow | undefined): boolean {
  if (!row || row.length === 0) return false
  // 最後一格是寬字的後半也算寫滿。
  const last = row[row.length - 1]
  return last === '' || last.trim() !== ''
}

/** 第 `y` 列上（含從上一列折下來、或折到下一列的）網址。`rowAt` 讀不到回 undefined。 */
export function linksOnRow(rowAt: (y: number) => CellRow | undefined, y: number): FoundLink[] {
  if (!rowAt(y)) return []
  let top = y
  while (top > 0 && y - top < MAX_SPAN && full(rowAt(top - 1))) top--
  let bottom = y
  while (bottom - y < MAX_SPAN && full(rowAt(bottom)) && rowAt(bottom + 1)) bottom++
  let text = ''
  const map: CellPos[] = []
  for (let r = top; r <= bottom; r++) {
    const row = rowAt(r) ?? []
    for (let c = 0; c < row.length; c++) {
      const ch = row[c] === '' ? '' : row[c] || ' '
      for (let i = 0; i < ch.length; i++) {
        text += ch[i]
        map.push({ x: c, y: r })
      }
    }
    // 沒寫滿的列是一行的結尾：補一個空白，網址不會黏到下一行。
    if (!full(row)) {
      text += ' '
      map.push({ x: row.length, y: r })
    }
  }
  const out: FoundLink[] = []
  for (const m of text.matchAll(URL_RE)) {
    const url = m[0].replace(/[.,;:!?)\]}>]+$/, '')
    if (url.length <= 'https://'.length) continue
    const start = map[m.index]
    const end = map[m.index + url.length - 1]
    if (start.y <= y && end.y >= y) out.push({ url, start, end })
  }
  return out
}
