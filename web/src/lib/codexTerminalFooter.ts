import { trimClippedTail } from './statusLineTail'

export interface CodexTerminalFooter {
  modelLine: string | null
  warnings: number | null
}

export function splitCodexTerminalFooter(text: string): { body: string; footer: CodexTerminalFooter | null } {
  const lines = text.split('\n')
  let modelLine: string | null = null
  let warnings: number | null = null
  let found = false
  for (let i = lines.length - 1; i >= 0; i--) {
    const row = lines[i].trim()
    if (!row) continue
    const warning = /^(?:\? for shortcuts\s+)?⚠ (\d+) warnings? · f2 to view$/.exec(row)
    if (warning) {
      warnings = Number(warning[1])
      found = true
    } else if (row === '? for shortcuts') {
    } else if (/^[a-z0-9][\w.-]*-[\w.-]+(?:\s+[^·]+)?\s*·\s*[~/]/i.test(row)) {
      const tail = /\s{2,}⚠ (\d+) warnings? · f2 to view$/.exec(row)
      if (tail) warnings = Number(tail[1])
      modelLine = tail ? row.slice(0, tail.index) : row
      found = true
      break
    } else {
      break
    }
  }
  return found ? { body: text, footer: { modelLine, warnings } } : { body: text, footer: null }
}

export interface TerminalFooterView {
  botId: string
  runId: string | null
  footer: CodexTerminalFooter | null
}

export interface CodexModelLineFields {
  model: string
  effort: string | null
  fast: boolean
  cwd: string | null
  contextUsedPct: number | null
  limits: { label: string; leftPct: number }[]
  title: string | null
}

export function parseCodexModelLine(modelLine: string): CodexModelLineFields {
  const [head = '', ...rest] = trimClippedTail(modelLine).split(/\s+·\s+/)
  const words = head.trim().split(/\s+/)
  const fast = words.slice(1).includes('fast')
  const effort = words.slice(1).find((w) => w !== 'fast') ?? null
  let cwd: string | null = null
  let contextUsedPct: number | null = null
  const limits: CodexModelLineFields['limits'] = []
  const title: string[] = []
  for (const seg of rest) {
    const s = seg.trim()
    const ctx = /^Context (\d+)% used$/i.exec(s)
    const left = /^(5h|weekly|[a-z0-9]+) (\d+)%(?: left)?$/i.exec(s)
    if (cwd === null && title.length === 0 && /^[~/]/.test(s)) cwd = s
    else if (ctx) contextUsedPct = Number(ctx[1])
    else if (left) limits.push({ label: left[1], leftPct: Number(left[2]) })
    else if (s) title.push(s)
  }
  return { model: words[0] ?? '', effort, fast, cwd, contextUsedPct, limits, title: title.length ? title.join(' · ') : null }
}
