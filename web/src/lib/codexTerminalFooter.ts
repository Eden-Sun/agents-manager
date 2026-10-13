/** Parse the contiguous CLI footer at the end for the status bar; the terminal keeps every line (#1680). */
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
    } else if (row === '? for shortcuts') {
      // Keyboard help is chrome, not another status field.
    } else if (/^[a-z0-9][\w.-]*-[\w.-]+(?:\s+[^·]+)?\s*·\s*[~/]/i.test(row)) {
      // The warning can sit on the model row itself, set apart by 2+ spaces (#1671).
      const tail = /\s{2,}⚠ (\d+) warnings? · f2 to view$/.exec(row)
      if (tail) warnings = Number(tail[1])
      modelLine = tail ? row.slice(0, tail.index) : row
      found = true
      break
    } else {
      break
    }
    found = true
  }
  return found ? { body: text, footer: { modelLine, warnings } } : { body: text, footer: null }
}

export interface TerminalFooterView {
  botId: string
  runId: string | null
  footer: CodexTerminalFooter | null
}

