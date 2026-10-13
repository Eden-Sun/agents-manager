/** Only peel the contiguous CLI footer at the end; historical output stays intact. */
export interface CodexTerminalFooter {
  modelLine: string | null
  warnings: number | null
}

export function splitCodexTerminalFooter(text: string): { body: string; footer: CodexTerminalFooter | null } {
  const lines = text.split('\n')
  let start = lines.length
  let modelLine: string | null = null
  let warnings: number | null = null
  let found = false
  for (let i = lines.length - 1; i >= 0; i--) {
    const row = lines[i].trim()
    if (!row) continue
    const warning = /^(?:\? for shortcuts\s+)?⚠ (\d+) warnings? · f2 to view$/.exec(row)
    if (warning) {
      warnings = Number(warning[1])
    } else if (row === '? for shortcuts') {
      // Keyboard help is chrome, not another status field.
    } else if (/^[a-z0-9][\w.-]*-[\w.-]+(?:\s+[^·]+)?\s*·\s*[~/]/i.test(row)) {
      modelLine = row
      start = i
      found = true
      break
    } else {
      break
    }
    start = i
    found = true
  }
  return found ? { body: lines.slice(0, start).join('\n').trimEnd(), footer: { modelLine, warnings } } : { body: text, footer: null }
}

export interface TerminalFooterView {
  botId: string
  runId: string | null
  footer: CodexTerminalFooter | null
}

