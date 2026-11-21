export type Piece = { text: string; url: string | null }

const URL_CHARS = /[^\s<>"'`）」』、。，；：！？》〉】〕“”‘’]/
const URL_RE = /https?:\/\/[^\s<>"'`）」』、。，；：！？》〉】〕“”‘’]+/g

const MIN_WRAP_WIDTH = 40

function continuesUrl(line: string | undefined): boolean {
  return line !== undefined && line.length > 0 && !/^\s/.test(line) && URL_CHARS.test(line[0])
}

function indentedTail(line: string | undefined): boolean {
  return line !== undefined && /^ [^\s]+$/.test(line) && URL_CHARS.test(line[1])
}

function isFullWrap(line: string | undefined, width: number): boolean {
  return line !== undefined && line.length === width && !/\s/.test(line) && continuesUrl(line)
}

function wrapsToNextLine(lines: string[], i: number, columns: number | null | undefined, longest: number): boolean {
  const width = lines[i].length
  if (width === 0) return false
  if (columns && columns > 0 && width > columns) return false
  if (indentedTail(lines[i + 1])) {
    if (columns && columns > 0) return width >= columns - 2
    return width >= MIN_WRAP_WIDTH && (lines[i + 1].length === width || width === longest)
  }
  if (!continuesUrl(lines[i + 1])) return false
  if (isFullWrap(lines[i + 1], width)) return true
  if (columns && columns > 0 && width === columns) return true
  if (columns && columns > 0) {
    const next = lines[i + 1]
    if (!next || /\s/.test(next)) return false
  }
  return width >= MIN_WRAP_WIDTH && width === longest
}

export function termPieces(text: string, columns?: number | null): Piece[][] {
  const lines = text.split('\n')
  const longest = Math.max(0, ...lines.map((l) => l.length))
  const rows: Piece[][] = []
  let carry: { url: string; frags: Piece[]; width: number } | null = null
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]
    const row: Piece[] = []
    let from = 0
    if (carry) {
      const indented = indentedTail(line)
      const m = indented ? /^ ([^\s]+)$/.exec(line) : /^[^\s]+/.exec(line)
      const tail = indented ? m?.[1] : m?.[0]
      if (m && tail && URL_CHARS.test(tail[0])) {
        const piece: Piece = { text: tail, url: null }
        carry.frags.push(piece)
        carry.url += tail
        if (indented) row.push({ text: ' ', url: null })
        row.push(piece)
        from = m[0].length
        if (from !== line.length || line.length !== carry.width) {
          for (const f of carry.frags) f.url = trimPunct(carry.url)
          carry = null
        }
      } else {
        for (const f of carry.frags) f.url = trimPunct(carry.url)
        carry = null
      }
    }
    if (from < line.length) {
      const rest = line.slice(from)
      let last = 0
      for (const m of rest.matchAll(URL_RE)) {
        const at = m.index ?? 0
        if (at > last) row.push({ text: rest.slice(last, at), url: null })
        const endsLine = at + m[0].length === rest.length && wrapsToNextLine(lines, i, columns, longest)
        const piece: Piece = { text: m[0], url: endsLine ? null : trimPunct(m[0]) }
        row.push(piece)
        if (endsLine) carry = { url: m[0], frags: [piece], width: line.length }
        last = at + m[0].length
      }
      if (last < rest.length) row.push({ text: rest.slice(last), url: null })
    }
    rows.push(row)
  }
  if (carry) for (const f of carry.frags) f.url = trimPunct(carry.url)
  return rows
}

const TRAILING_PUNCT = '.,;:!?'

export function trimPunctSteps(url: string): { end: number; steps: number } {
  const n = url.length
  const parenExcess = new Int32Array(n + 1)
  const bracketExcess = new Int32Array(n + 1)
  for (let i = 0; i < n; i++) {
    const ch = url[i]
    parenExcess[i + 1] = parenExcess[i] + (ch === ')' ? 1 : ch === '(' ? -1 : 0)
    bracketExcess[i + 1] = bracketExcess[i] + (ch === ']' ? 1 : ch === '[' ? -1 : 0)
  }
  let end = n
  let steps = 0
  let moved = true
  while (moved) {
    steps++
    const before = end
    while (end > 0 && TRAILING_PUNCT.includes(url[end - 1])) {
      end--
      steps++
    }
    while (end > 0 && url[end - 1] === ')' && parenExcess[end] > 0) {
      end--
      steps++
    }
    while (end > 0 && url[end - 1] === ']' && bracketExcess[end] > 0) {
      end--
      steps++
    }
    moved = end !== before
  }
  return { end, steps }
}

export function trimPunct(url: string): string {
  return url.slice(0, trimPunctSteps(url).end)
}
