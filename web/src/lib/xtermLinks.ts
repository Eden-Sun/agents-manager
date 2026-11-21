
export type CellRow = string[]

export interface CellPos {
  x: number
  y: number
}

export interface FoundLink {
  url: string
  start: CellPos
  end: CellPos
}

const MAX_SPAN = 12
const URL_RE = /https?:\/\/[^\s<>"'`）」』、。，；：！？》〉】〕“”‘’]+/g

function usedWidth(row: CellRow): number {
  for (let i = row.length - 1; i >= 0; i--) if (row[i] === '' || row[i].trim() !== '') return i + 1
  return 0
}

export const WRAP_SLACK = 4

export function wrapWidth(rows: CellRow[], cols: number): number | null {
  let widest = 0
  for (const r of rows) widest = Math.max(widest, usedWidth(r))
  return widest >= cols - WRAP_SLACK ? widest : null
}

function full(row: CellRow | undefined, wrapAt: number | null | undefined): boolean {
  if (!row || row.length === 0 || wrapAt === null) return false
  return usedWidth(row) >= Math.min(wrapAt ?? row.length, row.length)
}

export function linksOnRow(rowAt: (y: number) => CellRow | undefined, y: number, wrapAt?: number | null): FoundLink[] {
  if (!rowAt(y)) return []
  let top = y
  while (top > 0 && y - top < MAX_SPAN && full(rowAt(top - 1), wrapAt)) top--
  let bottom = y
  while (bottom - y < MAX_SPAN && full(rowAt(bottom), wrapAt) && rowAt(bottom + 1)) bottom++
  let text = ''
  const map: CellPos[] = []
  for (let r = top; r <= bottom; r++) {
    const row = rowAt(r) ?? []
    const isFull = full(row, wrapAt)
    for (let c = 0, end = isFull ? usedWidth(row) : row.length; c < end; c++) {
      const ch = row[c] === '' ? '' : row[c] || ' '
      for (let i = 0; i < ch.length; i++) {
        text += ch[i]
        map.push({ x: c, y: r })
      }
    }
    if (!isFull) {
      text += ' '
      map.push({ x: row.length, y: r })
    }
  }
  const out: FoundLink[] = []
  for (const m of text.matchAll(URL_RE)) {
    const url = m[0].replace(/[.,;:!?)\]}>]+$/, '')
    if (url.length <= 'https://'.length) continue
    const start = map[m.index]
    const end = map[m.index + url.length - 1]
    if (start.y <= y && end.y >= y) out.push({ url, start, end })
  }
  return out
}

export interface FoundCode {
  code: string
  start: CellPos
  end: CellPos
}

const CODE_RE = /(?<![A-Za-z0-9-])[A-Z0-9]{4,5}-[A-Z0-9]{4,5}(?![A-Za-z0-9-])/g

export function codesOnRow(row: CellRow, y: number): FoundCode[] {
  let text = ''
  const xs: number[] = []
  row.forEach((ch, x) => {
    for (const c of ch) {
      text += c
      xs.push(x)
    }
  })
  const out: FoundCode[] = []
  for (const m of text.matchAll(CODE_RE)) {
    if (!/[A-Z]/.test(m[0]) || !/[0-9]/.test(m[0])) continue
    out.push({ code: m[0], start: { x: xs[m.index], y }, end: { x: xs[m.index + m[0].length - 1], y } })
  }
  return out
}
