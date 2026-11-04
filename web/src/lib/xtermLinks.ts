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

/** 這一列用到第幾格（含寬字的後半）；全空白是 0。 */
function usedWidth(row: CellRow): number {
  for (let i = row.length - 1; i >= 0; i--) if (row[i] === '' || row[i].trim() !== '') return i + 1
  return 0
}

/**
 * herdr 的 pane 實際寬度比 xterm 的欄數少 1～3 格（pane 的捲軸／邊框那幾格；xterm 欄數取自 herdr 的版面矩形，
 * 量不到真正的欄數）。折行的列只會寫到 pane 的寬度，最後幾格永遠是空的，所以「寫滿」不能只看最後一格。
 */
export const WRAP_SLACK = 4

/**
 * 折行的寬度：畫面上用得最寬的那一列。折行的列一定寫滿 pane 的寬度（任何列都不會比它寬），
 * 所以畫面上有折行時，最寬的那列就是 pane 寬；最寬的列離右邊還很遠（超過 [`WRAP_SLACK`]）代表畫面上沒有折行，回傳 `null`＝沒有列算寫滿。
 */
export function wrapWidth(rows: CellRow[], cols: number): number | null {
  let widest = 0
  for (const r of rows) widest = Math.max(widest, usedWidth(r))
  return widest >= cols - WRAP_SLACK ? widest : null
}

function full(row: CellRow | undefined, wrapAt: number | null | undefined): boolean {
  if (!row || row.length === 0 || wrapAt === null) return false
  return usedWidth(row) >= Math.min(wrapAt ?? row.length, row.length)
}

/** 第 `y` 列上（含從上一列折下來、或折到下一列的）網址。`rowAt` 讀不到回 undefined。 */
export function linksOnRow(rowAt: (y: number) => CellRow | undefined, y: number, wrapAt?: number | null): FoundLink[] {
  // `wrapAt`：用到第幾格才算寫滿（[`wrapWidth`]）；不給＝寫滿到最後一格；`null`＝沒有折行。
  if (!rowAt(y)) return []
  let top = y
  while (top > 0 && y - top < MAX_SPAN && full(rowAt(top - 1), wrapAt)) top--
  let bottom = y
  while (bottom - y < MAX_SPAN && full(rowAt(bottom), wrapAt) && rowAt(bottom + 1)) bottom++
  let text = ''
  const map: CellPos[] = []
  for (let r = top; r <= bottom; r++) {
    const row = rowAt(r) ?? []
    // 折行的列最後幾格是空的（pane 比 xterm 窄），不能算進網址。
    const isFull = full(row, wrapAt)
    for (let c = 0, end = isFull ? usedWidth(row) : row.length; c < end; c++) {
      const ch = row[c] === '' ? '' : row[c] || ' '
      for (let i = 0; i < ch.length; i++) {
        text += ch[i]
        map.push({ x: c, y: r })
      }
    }
    // 沒寫滿的列是一行的結尾：補一個空白，網址不會黏到下一行。
    if (!isFull) {
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

export interface FoundCode {
  code: string
  start: CellPos
  /** 含這一格。 */
  end: CellPos
}

/**
 * 登入用的一次性碼（codex 裝置碼 `WL4L-OTCJ9`、gh 的 `ABCD-1234` 之類）：兩段 4～5 個大寫英數字、中間一個 `-`，
 * 前後不黏英數字或 `-`，且同時有字母和數字（排除 `TODO-LIST`、日期之類）。點了複製用。
 */
const CODE_RE = /(?<![A-Za-z0-9-])[A-Z0-9]{4,5}-[A-Z0-9]{4,5}(?![A-Za-z0-9-])/g

export function codesOnRow(row: CellRow, y: number): FoundCode[] {
  let text = ''
  const xs: number[] = []
  row.forEach((ch, x) => {
    for (const c of ch) {
      text += c
      xs.push(x)
    }
  })
  const out: FoundCode[] = []
  for (const m of text.matchAll(CODE_RE)) {
    if (!/[A-Z]/.test(m[0]) || !/[0-9]/.test(m[0])) continue
    out.push({ code: m[0], start: { x: xs[m.index], y }, end: { x: xs[m.index + m[0].length - 1], y } })
  }
  return out
}
