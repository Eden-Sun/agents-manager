
export type ShellOp = { kind: 'text'; text: string } | { kind: 'keys'; keys: string[] }

const PASTE_START = '\x1b[200~'
const PASTE_END = '\x1b[201~'

const TILDE_KEYS: Record<number, string> = {
  11: 'f1', 12: 'f2', 13: 'f3', 14: 'f4', 15: 'f5',
  17: 'f6', 18: 'f7', 19: 'f8', 20: 'f9', 21: 'f10',
  23: 'f11', 24: 'f12',
}

const LETTER_KEYS: Record<string, string> = {
  A: 'up', B: 'down', C: 'right', D: 'left',
  P: 'f1', Q: 'f2', R: 'f3', S: 'f4',
}

const SS3_KEYS: Record<string, string> = {
  A: 'up', B: 'down', C: 'right', D: 'left',
  P: 'f1', Q: 'f2', R: 'f3', S: 'f4',
}

function modPrefix(mod: number): string {
  const bits = mod - 1
  return (bits & 4 ? 'ctrl+' : '') + (bits & 2 ? 'alt+' : '') + (bits & 1 ? 'shift+' : '')
}

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
  if (parts.length === 1 || (parts.length === 2 && parts[0] !== '1')) return null
  return modPrefix(mod) + name
}

function ctrlName(code: number): string {
  return `ctrl+${String.fromCharCode(code + 96)}`
}

function parseEscape(data: string, i: number): { op: ShellOp; len: number } {
  const raw = (len: number): { op: ShellOp; len: number } => ({ op: { kind: 'text', text: data.slice(i, i + len) }, len })
  const key = (name: string, len: number): { op: ShellOp; len: number } => ({ op: { kind: 'keys', keys: [name] }, len })

  const next = data[i + 1]
  if (next === undefined) return key('esc', 1)

  if (next === '[') {
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

  const cp = data.codePointAt(i + 1) as number
  const ch = String.fromCodePoint(cp)
  const len = 1 + ch.length
  if (ch === '\r') return key('alt+enter', len)
  if (ch === '\t') return key('alt+tab', len)
  if (ch === '\x7f' || ch === '\b') return key('alt+backspace', len)
  if (ch === ' ') return key('alt+space', len)
  if (cp >= 1 && cp <= 26) return key(`alt+${ctrlName(cp)}`, len)
  if (cp < 0x20) return key('esc', 1)
  return key(`alt+${ch}`, len)
}

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
    else pushText(ch)
  }
  return ops
}

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
    pending += data
    if (!scheduled) {
      scheduled = true
      schedule(flush)
    }
  }
}
