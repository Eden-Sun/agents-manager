/**
 * 主機 shell 的 ANSI 畫面怎麼畫進 xterm（純函式，沒有 DOM，給測試直接跑）。
 *
 * 目標三件事：
 * - 只重畫變了的列（每一列先 `\x1b[2K` 清掉再寫，不整屏清，所以不閃）；
 * - 整張畫面往上捲（新輸出把舊的頂上去）時，被捲出去的列要真的進 xterm 的捲動歷史，回看才看得到；
 * - 整批寫進一次 `term.write`，中間不會露出半張畫面。
 */

/** 把 herdr 的畫面文字切成列；尾端的換行不算一列；有給 `rows` 就補空列到那個高度。 */
export function splitFrame(text: string, rows: number | null): string[] {
  const body = text.endsWith('\n') ? text.slice(0, -1) : text
  const lines = body === '' ? [] : body.split('\n')
  if (rows && lines.length > rows) return lines.slice(lines.length - rows)
  if (rows) while (lines.length < rows) lines.push('')
  return lines
}

export interface FramePlan {
  /** 要寫進 xterm 的 VT 序列（可能是空字串＝畫面沒變）。 */
  out: string
  /** 寫完之後 xterm 視窗裡的列；下一張拿它當 `prev`。 */
  lines: string[]
}

const pos = (row: number) => `\x1b[${row + 1};1H\x1b[2K`
const SGR_RESET = '\x1b[0m'
/** 去掉 CSI 序列後是不是空白列（空白列不算「內容相符」，免得兩張空白畫面被誤判成捲動）。 */
// 去 CSI 序列要用控制字元 ESC，這裡是刻意的。
// oxlint-disable-next-line no-control-regex
const isBlank = (line: string) => line.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, '').trim() === ''

/**
 * `prev`＝xterm 視窗目前的列，`next`＝這一張；兩者長度一樣才做增量（長度不同＝resize，整屏重畫）。
 *
 * 捲動偵測：找一個位移 s 讓 `next[i] === prev[i + s]` 的非空白列最多（s=0 就是原地重畫）。
 * 只有 s>0 的命中數**多過** s=0 才當成「往上捲了 s 列」：先在最底列換行 s 次，把頂上的 s 列推進捲動歷史，
 * 再把剩下的差異列原地重畫。
 */
export function planFrame(prev: string[], next: string[]): FramePlan {
  const n = next.length
  if (prev.length !== n || n === 0) {
    return fullRedraw(next)
  }
  const score = (s: number) => {
    let hit = 0
    for (let i = 0; i + s < n; i++) if (next[i] === prev[i + s] && !isBlank(next[i])) hit++
    return hit
  }
  const score0 = score(0)
  let best = 0
  let bestScore = score0
  for (let s = 1; s < n; s++) {
    const sc = score(s)
    if (sc > bestScore) {
      best = s
      bestScore = sc
    }
  }
  let out = ''
  let cur = prev
  if (best > 0) {
    out += `\x1b[${n};1H` + '\r\n'.repeat(best)
    cur = [...prev.slice(best), ...Array<string>(best).fill('')]
  }
  const changed: number[] = []
  for (let i = 0; i < n; i++) {
    if (cur[i] !== next[i]) changed.push(i)
  }
  // 變動太多（全螢幕程式：vim、less、top…）就不做增量：增量很容易留下殘影（舊列沒被清到），整屏重畫才保證正確。
  if (best === 0 && changed.length * 3 > n) return fullRedraw(next)
  for (const i of changed) out += pos(i) + next[i] + SGR_RESET
  return { out, lines: next }
}

/** 整屏重畫：清掉整個視窗再從頭畫（回捲動歷史不動）。 */
function fullRedraw(next: string[]): FramePlan {
  return {
    out: `\x1b[H\x1b[2J` + next.map((line, i) => pos(i) + line + SGR_RESET).join(''),
    lines: next,
  }
}

/** 顯示寬度（去 SGR、中日韓全形算 2）。 */
function displayWidth(line: string): number {
  const plain = line.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, '')
  let w = 0
  for (const ch of plain) {
    const c = ch.codePointAt(0) ?? 0
    const wide =
      (c >= 0x1100 && c <= 0x115f) ||
      (c >= 0x2e80 && c <= 0xa4cf) ||
      (c >= 0xac00 && c <= 0xd7a3) ||
      (c >= 0xf900 && c <= 0xfaff) ||
      (c >= 0xfe30 && c <= 0xfe4f) ||
      (c >= 0xff00 && c <= 0xff60) ||
      (c >= 0xffe0 && c <= 0xffe6)
    w += wide ? 2 : 1
  }
  return w
}

/**
 * 游標擺哪：herdr 的畫面不帶游標位置，所以放在最後一個有字的列的字尾（shell 的提示後面就是打字的地方）。
 * 全部是空白＝擺在左上。
 */
export function cursorSeq(lines: string[]): string {
  let row = lines.length - 1
  while (row > 0 && isBlank(lines[row])) row--
  if (row < 0 || isBlank(lines[row])) return '\x1b[1;1H\x1b[?25h'
  // TUI 的畫面文字裡可能夾了「隱藏游標」，這裡一律打開。
  return `\x1b[${row + 1};${displayWidth(lines[row]) + 1}H\x1b[?25h`
}
