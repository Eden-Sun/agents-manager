/**
 * claude AskUserQuestion 帶 `preview` 時的並排版面（2026-10-12 console-pm，213×55，issue #1473）：
 * 左欄是編號選項，右欄是**游標所在那一項**的預覽框（`┌─┐ │ │ └─┘`），框底下一行 `Notes: press n to add notes`。
 *
 *   ❯ 1. 改回 200 正常訊息            ┌──────────┐
 *       (Recommended)                 │ HTTP 200 │
 *     2. 維持錯誤，只改訊息           │ {        │
 *                                     └──────────┘
 *                                     Notes: press n to add notes
 *
 * `parseChoiceMenu` 是逐行解析：預覽框的內文會被當成畫面底的雜行（超過 `TAIL_LIMIT` 就整份認不出），
 * 框裡剛好有 `1. …` 還會被當成選項。這裡先把右欄切掉：回傳只剩左欄的行（行數不變、行號對得上）與預覽文字。
 * 框的欄位用「終端格寬」對（CJK 佔兩格），不能用字串索引——左欄有中文時每行的 `│` 索引都不同。
 */

export interface PreviewSplit {
  /** 與輸入等長；框內那幾行只留左欄，`Notes:` 提示行清空。 */
  lines: string[]
  /** 框內文字：去掉框線與一格內距、保留縮排、尾端空白行已去。 */
  preview: string
}

/** 終端格寬：CJK 全形字、全形標點與 emoji 佔兩格，其餘一格（組合字元不處理，pane 內文用不到）。 */
function cellWidth(cp: number): number {
  return (cp >= 0x1100 && cp <= 0x115f) ||
    (cp >= 0x2e80 && cp <= 0xa4cf) ||
    (cp >= 0xac00 && cp <= 0xd7a3) ||
    (cp >= 0xf900 && cp <= 0xfaff) ||
    (cp >= 0xfe30 && cp <= 0xfe6f) ||
    (cp >= 0xff00 && cp <= 0xff60) ||
    (cp >= 0xffe0 && cp <= 0xffe6) ||
    (cp >= 0x1f300 && cp <= 0x1faff) ||
    (cp >= 0x20000 && cp <= 0x3fffd)
    ? 2
    : 1
}

/** `col` 這一格起頭的字元在字串裡的索引；那格落在雙格字中間或超出行尾回 -1。 */
function indexAtCell(line: string, col: number): number {
  let at = 0
  let i = 0
  for (const ch of line) {
    if (at === col) return i
    if (at > col) return -1
    at += cellWidth(ch.codePointAt(0) ?? 0)
    i += ch.length
  }
  return -1
}

/** 字串索引 `idx` 之前佔了幾格。 */
function cellsBefore(line: string, idx: number): number {
  let at = 0
  let i = 0
  for (const ch of line) {
    if (i >= idx) break
    at += cellWidth(ch.codePointAt(0) ?? 0)
    i += ch.length
  }
  return at
}

/** 框頂：左欄（選項行或空）、至少兩個空白、`┌` 接一串橫線。 */
const TOP = /^(.*?)\s{2,}┌─{3,}/

/** 左欄只能是空白或編號選項（`❯ 1. …`），別的畫面（例如 diff 框）不碰。 */
const LEFT_OK = /^(\s*|\s*([❯›▶>]\s)?\d{1,2}[.)]\s+\S.*)$/

const NOTES = /^\s*Notes:\s/i

/** 認不出並排版面回 `null`，呼叫端照原樣解析。 */
export function splitPreviewPane(lines: string[]): PreviewSplit | null {
  // 由下往上找最後一個框頂。
  let top = -1
  for (let i = lines.length - 1; i >= 0; i--) {
    const m = TOP.exec(lines[i])
    if (m && LEFT_OK.test(m[1])) {
      top = i
      break
    }
  }
  if (top < 0) return null

  const topIdx = lines[top].indexOf('┌')
  const col = cellsBefore(lines[top], topIdx)

  const out = lines.slice()
  out[top] = lines[top].slice(0, topIdx).replace(/\s+$/, '')
  const body: string[] = []
  let bottom = lines.length - 1
  let closed = false
  for (let i = top + 1; i < lines.length; i++) {
    const at = indexAtCell(lines[i], col)
    const ch = at >= 0 ? lines[i][at] : ''
    if (ch === '│') {
      let inner = lines[i].slice(at + 1)
      if (inner.startsWith(' ')) inner = inner.slice(1)
      inner = inner.replace(/\s+$/, '')
      if (inner.endsWith('│')) inner = inner.slice(0, -1).replace(/\s+$/, '')
      body.push(inner)
      out[i] = lines[i].slice(0, at).replace(/\s+$/, '')
      continue
    }
    if (ch === '└') {
      out[i] = lines[i].slice(0, at).replace(/\s+$/, '')
      bottom = i
      closed = true
      break
    }
    // 框在這一行斷掉（pane 截尾或別的畫面）：到此為止。
    bottom = i - 1
    break
  }
  // 沒有任何框內文字＝只是巧合長得像，不動。
  if (!body.length || (!closed && bottom === top)) return null

  // 框底下的 `Notes: press n to add notes`（在框的欄位上）不是選項也不是預覽。
  for (let i = bottom + 1; i < lines.length && i <= bottom + 3; i++) {
    const s = lines[i]
    if (s.trim() === '') continue
    if (NOTES.test(s) && cellsBefore(s, s.length - s.trimStart().length) >= col) out[i] = ''
    break
  }

  while (body.length && body[body.length - 1] === '') body.pop()
  return { lines: out, preview: body.join('\n') }
}
