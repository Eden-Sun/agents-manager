
export interface PreviewSplit {
  lines: string[]
  preview: string
}

function codePointCellWidth(cp: number): number {
  return (cp >= 0x1100 && cp <= 0x115f) ||
    (cp >= 0x2e80 && cp <= 0xa4cf) ||
    (cp >= 0xac00 && cp <= 0xd7a3) ||
    (cp >= 0xf900 && cp <= 0xfaff) ||
    (cp >= 0xfe30 && cp <= 0xfe6f) ||
    (cp >= 0xff00 && cp <= 0xff60) ||
    (cp >= 0xffe0 && cp <= 0xffe6) ||
    (cp >= 0x1f300 && cp <= 0x1faff) ||
    (cp >= 0x20000 && cp <= 0x3fffd) ||
    /\p{Emoji_Presentation}/u.test(String.fromCodePoint(cp))
    ? 2
    : 1
}

const graphemes = new Intl.Segmenter(undefined, { granularity: 'grapheme' })
const MARK = /^\p{Mark}$/u

function cellWidth(cluster: string): number {
  let width = 0
  for (const ch of cluster) {
    if (ch === '\u200d' || MARK.test(ch)) continue
    width = Math.max(width, codePointCellWidth(ch.codePointAt(0) ?? 0))
  }
  const codePoints = [...cluster].map((ch) => ch.codePointAt(0) ?? 0)
  const regionalIndicators = codePoints.filter((cp) => cp >= 0x1f1e6 && cp <= 0x1f1ff)
  if (regionalIndicators.length >= 2 || codePoints.includes(0xfe0f) || codePoints.includes(0x20e3)) {
    return 2
  }
  return width
}

function indexAtCell(line: string, col: number): number {
  let at = 0
  for (const { index, segment } of graphemes.segment(line)) {
    if (at === col) return index
    if (at > col) return -1
    at += cellWidth(segment)
  }
  return -1
}

function cellsBefore(line: string, idx: number): number {
  let at = 0
  for (const { index, segment } of graphemes.segment(line)) {
    if (index >= idx) break
    at += cellWidth(segment)
  }
  return at
}

const TOP = /^(.*?)\s{2,}┌─{3,}/

const LEFT_OK = /^(\s*|\s*([❯›▶>]\s)?\d{1,2}[.)]\s+\S.*)$/

const NOTES = /^\s*Notes:\s/i

export type PreviewSplitResult = PreviewSplit | { malformed: true } | null

export function splitPreviewPane(lines: string[]): PreviewSplitResult {
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
    if (lines[i].includes('│') || lines[i].includes('└')) return { malformed: true }
    bottom = i - 1
    break
  }
  if (!body.length || (!closed && bottom === top)) return null

  for (let i = bottom + 1; i < lines.length && i <= bottom + 3; i++) {
    const s = lines[i]
    if (s.trim() === '') continue
    if (NOTES.test(s) && cellsBefore(s, s.length - s.trimStart().length) >= col) out[i] = ''
    break
  }

  while (body.length && body[body.length - 1] === '') body.pop()
  return { lines: out, preview: body.join('\n') }
}
