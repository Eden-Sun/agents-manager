
import { ESC } from './charCodes'

const ANSI_SGR = new RegExp(`${ESC}\\[[0-9;?]*[ -/]*[@-~]`, 'g')

export function splitFrame(text: string, rows: number | null): string[] {
  const body = text.endsWith('\n') ? text.slice(0, -1) : text
  const lines = body === '' ? [] : body.split('\n')
  if (rows && lines.length > rows) return lines.slice(lines.length - rows)
  if (rows) while (lines.length < rows) lines.push('')
  return lines
}

export interface FramePlan {
  out: string
  lines: string[]
}

const pos = (row: number) => `\x1b[${row + 1};1H\x1b[2K`
const SGR_RESET = '\x1b[0m'
const isBlank = (line: string) => line.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, '').trim() === ''

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
  if (best === 0 && changed.length * 3 > n) return fullRedraw(next)
  for (const i of changed) out += pos(i) + next[i] + SGR_RESET
  return { out, lines: next }
}

function fullRedraw(next: string[]): FramePlan {
  return {
    out: `\x1b[H\x1b[2J` + next.map((line, i) => pos(i) + line + SGR_RESET).join(''),
    lines: next,
  }
}

function displayWidth(line: string): number {
  const plain = line.replace(ANSI_SGR, '')
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

export function cursorSeq(lines: string[], typedSpaces = 0): string {
  let row = lines.length - 1
  while (row > 0 && isBlank(lines[row])) row--
  if (row < 0 || isBlank(lines[row])) return '\x1b[1;1H\x1b[?25h'
  const plain = lines[row].replace(ANSI_SGR, '')
  const trimmed = plain.replace(/ +$/, '')
  const col = displayWidth(trimmed) + Math.min(typedSpaces, plain.length - trimmed.length)
  return `\x1b[${row + 1};${col + 1}H\x1b[?25h`
}

export function typedSpacesAfter(prev: number, op: { kind: 'text'; text: string } | { kind: 'keys'; keys: string[] }): number {
  if (op.kind === 'text') {
    if (/[\r\n]/.test(op.text)) return 0
    const tail = op.text.length - op.text.replace(/ +$/, '').length
    return tail === op.text.length ? prev + tail : tail
  }
  let n = prev
  for (const k of op.keys) n = k === 'backspace' ? Math.max(0, n - 1) : 0
  return n
}
