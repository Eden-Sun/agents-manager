/**
 * 主機 shell 面板（xterm.js）的輸入轉換：xterm 的 `onData` 字串 → 依序的 herdr 操作。
 *
 * - 一般字元、多位元組 UTF-8、貼上 → `text`（`pane.send_text`，herdr 眼中的貼上）。
 * - 控制序列 → `keys`（`pane.send_keys` 的鍵名）。鍵名照 [`herdrKeyFromEvent`]（hooks/usePaneKeys.ts）
 *   與 herdr 鍵盤設定的寫法：`enter`、`esc`、`tab`、`backspace`、`up`…、`space`、`f1`…`f12`、`ctrl+a`、
 *   `shift+tab`、`alt+x`，修飾鍵順序 `ctrl+alt+shift+`。
 * - herdr 沒有可靠鍵名的序列（Home／End／Insert／Delete／PageUp／PageDown、F13 以上、Ctrl-\ 等）
 *   **退回當 text 原樣送**：沒有鍵名可以驗證，硬送一個 herdr 不認得的鍵會整批被 400 拒絕。
 * - text 裡的 `\r` 一律轉成 `enter` 鍵（`\r\n` 也是一次 Enter）；`\n` 留在 text 裡當換行貼上。
 */

export type ShellOp = { kind: 'text'; text: string } | { kind: 'keys'; keys: string[] }

const PASTE_START = '\x1b[200~'
const PASTE_END = '\x1b[201~'

/** `CSI <n> ~` 的功能鍵（xterm 慣例）。Home／End／Insert／Delete／PgUp／PgDn 沒有可驗證的 herdr 鍵名，不在表內。 */
const TILDE_KEYS: Record<number, string> = {
  11: 'f1', 12: 'f2', 13: 'f3', 14: 'f4', 15: 'f5',
  17: 'f6', 18: 'f7', 19: 'f8', 20: 'f9', 21: 'f10',
  23: 'f11', 24: 'f12',
}

/** `CSI <final>`（無數字參數或 `1;<mod>`）的鍵名。H／F（Home／End）不在表內，退回 text。 */
const LETTER_KEYS: Record<string, string> = {
  A: 'up', B: 'down', C: 'right', D: 'left',
  P: 'f1', Q: 'f2', R: 'f3', S: 'f4',
}

/** `SS3 <final>`（`ESC O`，應用程式游標模式）的鍵名。 */
const SS3_KEYS: Record<string, string> = {
  A: 'up', B: 'down', C: 'right', D: 'left',
  P: 'f1', Q: 'f2', R: 'f3', S: 'f4',
}

/** xterm 修飾鍵參數（1 = 無；其餘 = 1 + 位元：1 shift、2 alt、4 ctrl）→ 鍵名前綴。 */
function modPrefix(mod: number): string {
  const bits = mod - 1
  return (bits & 4 ? 'ctrl+' : '') + (bits & 2 ? 'alt+' : '') + (bits & 1 ? 'shift+' : '')
}

/** `CSI` 之後的內容（參數 + 最後一個位元組）→ herdr 鍵名；認不得就回 null。 */
function csiKey(body: string): string | null {
  const final = body[body.length - 1]
  const params = body.slice(0, -1)
  if (!/^(\d+(;\d+)?)?$/.test(params)) return null
  const parts = params === '' ? [] : params.split(';')
  const mod = parts.length === 2 ? Number(parts[1]) : 1
  if (mod < 1) return null

  if (final === 'Z') return parts.length === 0 ? 'shift+tab' : null
  if (final === '~') {
    const name = parts.length >= 1 ? TILDE_KEYS[Number(parts[0])] : undefined
    return name ? modPrefix(mod) + name : null
  }
  const name = LETTER_KEYS[final]
  if (!name) return null
  // 方向鍵與 F1–F4 帶修飾鍵時是 `1;<mod>`；其他形式不認。
  if (parts.length === 1 || (parts.length === 2 && parts[0] !== '1')) return null
  return modPrefix(mod) + name
}

/** 控制字元 1–26（Ctrl-A..Z）→ `ctrl+a`..`ctrl+z`；Tab、Enter、Backspace（\b）不在這裡。 */
function ctrlName(code: number): string {
  return `ctrl+${String.fromCharCode(code + 96)}`
}

/** 解析 `i` 起始的 ESC 序列；回傳操作與吃掉的長度。 */
function parseEscape(data: string, i: number): { op: ShellOp; len: number } {
  const raw = (len: number): { op: ShellOp; len: number } => ({ op: { kind: 'text', text: data.slice(i, i + len) }, len })
  const key = (name: string, len: number): { op: ShellOp; len: number } => ({ op: { kind: 'keys', keys: [name] }, len })

  const next = data[i + 1]
  if (next === undefined) return key('esc', 1)

  if (next === '[') {
    // CSI：參數與中介位元組（0x20–0x3f）之後接最後一個位元組（0x40–0x7e）。
    let j = i + 2
    while (j < data.length && data[j] >= ' ' && data[j] <= '?') j++
    if (j >= data.length) return raw(data.length - i)
    if (data[j] < '@' || data[j] > '~') return raw(j - i)
    const name = csiKey(data.slice(i + 2, j + 1))
    return name ? key(name, j + 1 - i) : raw(j + 1 - i)
  }

  if (next === 'O') {
    const f = data[i + 2]
    if (f === undefined) return raw(2)
    const name = SS3_KEYS[f]
    return name ? key(name, 3) : raw(3)
  }

  if (next === '\x1b') return key('esc', 1)

  // Alt + 字元。
  const cp = data.codePointAt(i + 1) as number
  const ch = String.fromCodePoint(cp)
  const len = 1 + ch.length
  if (ch === '\r') return key('alt+enter', len)
  if (ch === '\t') return key('alt+tab', len)
  if (ch === '\x7f' || ch === '\b') return key('alt+backspace', len)
  if (ch === ' ') return key('alt+space', len)
  if (cp >= 1 && cp <= 26) return key(`alt+${ctrlName(cp)}`, len)
  // 其他控制字元：先吐 Esc，字元留給下一輪。
  if (cp < 0x20) return key('esc', 1)
  return key(`alt+${ch}`, len)
}

/** xterm `onData` 的字串 → 依序的操作；相鄰同類合併。 */
export function dataToOps(data: string): ShellOp[] {
  const ops: ShellOp[] = []

  const pushText = (text: string): void => {
    if (!text) return
    const last = ops[ops.length - 1]
    if (last?.kind === 'text') last.text += text
    else ops.push({ kind: 'text', text })
  }
  const pushKey = (name: string): void => {
    const last = ops[ops.length - 1]
    if (last?.kind === 'keys') last.keys.push(name)
    else ops.push({ kind: 'keys', keys: [name] })
  }
  const pushOp = (op: ShellOp): void => {
    if (op.kind === 'text') pushText(op.text)
    else for (const k of op.keys) pushKey(k)
  }

  let i = 0
  while (i < data.length) {
    if (data.startsWith(PASTE_START, i)) {
      const start = i + PASTE_START.length
      const end = data.indexOf(PASTE_END, start)
      const body = end < 0 ? data.slice(start) : data.slice(start, end)
      // 貼上整段當 text；\r／\r\n 是換行，轉成 Enter 鍵，\n 留在 text 裡。
      body.split(/\r\n?/).forEach((part, idx) => {
        if (idx > 0) pushKey('enter')
        pushText(part)
      })
      i = end < 0 ? data.length : end + PASTE_END.length
      continue
    }

    if (data[i] === '\x1b') {
      const r = parseEscape(data, i)
      pushOp(r.op)
      i += r.len
      continue
    }

    const ch = String.fromCodePoint(data.codePointAt(i) as number)
    const code = ch.codePointAt(0) as number
    i += ch.length

    if (ch === '\r') pushKey('enter')
    else if (ch === '\x7f' || ch === '\b') pushKey('backspace')
    else if (ch === '\t') pushKey('tab')
    else if (code >= 1 && code <= 26) pushKey(ctrlName(code))
    // 其餘（一般字元、多位元組字元，以及 Ctrl-@、Ctrl-\、Ctrl-]、Ctrl-^、Ctrl-_ 這幾個 herdr 沒有可驗證鍵名的控制字元）都當 text 原樣送。
    else pushText(ch)
  }
  return ops
}

/**
 * 同一個 animation frame 內的多次 `onData` 合併成一次 `send`（一次 HTTP 請求，順序不變）。
 * 空字串不送；`schedule` 預設 requestAnimationFrame（沒有時退回 setTimeout），測試可注入。
 */
export function createInputBatcher(
  send: (ops: ShellOp[]) => void,
  schedule: (cb: () => void) => void = (cb) =>
    typeof requestAnimationFrame === 'function' ? requestAnimationFrame(() => cb()) : setTimeout(cb, 0),
): (data: string) => void {
  let pending = ''
  let scheduled = false

  const flush = (): void => {
    scheduled = false
    const data = pending
    pending = ''
    if (!data) return
    const ops = dataToOps(data)
    if (ops.length > 0) send(ops)
  }

  return (data: string): void => {
    if (!data) return
    // 原始字串先串起來再解析：一個序列被切在兩次 onData 之間也不會拆壞。
    pending += data
    if (!scheduled) {
      scheduled = true
      schedule(flush)
    }
  }
}
