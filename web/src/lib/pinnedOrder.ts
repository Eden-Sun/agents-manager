
export interface Positioned {
  id: string
  position: number
  index: number
}

export function sortPinned<T extends Positioned>(items: T[]): T[] {
  return [...items].sort((a, b) => {
    const aPosition = a.position === 0 ? Number.POSITIVE_INFINITY : a.position
    const bPosition = b.position === 0 ? Number.POSITIVE_INFINITY : b.position
    return aPosition - bPosition || a.index - b.index
  })
}

export function moveBefore(order: string[], id: string, beforeId: string | null): string[] | null {
  if (id === beforeId || !order.includes(id)) return null
  const rest = order.filter((x) => x !== id)
  const at = beforeId === null ? rest.length : rest.indexOf(beforeId)
  if (beforeId !== null && at < 0) return null
  const next = [...rest.slice(0, at), id, ...rest.slice(at)]
  return next.join('\n') === order.join('\n') ? null : next
}

export function moveStep(order: string[], id: string, dir: -1 | 1): string[] | null {
  const at = order.indexOf(id)
  const to = at + dir
  if (at < 0 || to < 0 || to >= order.length) return null
  const next = [...order]
  ;[next[at], next[to]] = [next[to], next[at]]
  return next
}

export interface Box {
  id: string
  left: number
  top: number
  right: number
  bottom: number
}

export const DROP_STICKY_PX = 16

export interface Slot {
  before: string | null
  shift: string[]
  after: string | null
}

export function pickSlot(boxes: Box[], x: number, y: number, dragId: string, prev?: string | null): Slot {
  const others = boxes.filter((b) => b.id !== dragId)
  if (others.length === 0) return { before: null, shift: [], after: null }
  const rows: Box[][] = []
  for (const b of [...others].sort((p, q) => p.top - q.top || p.left - q.left)) {
    const row = rows[rows.length - 1]
    if (row && Math.abs(row[0].top - b.top) < (b.bottom - b.top) / 2) row.push(b)
    else rows.push([b])
  }
  const dist = (row: Box[]) => {
    const top = Math.min(...row.map((b) => b.top))
    const bottom = Math.max(...row.map((b) => b.bottom))
    return y < top ? top - y : y > bottom ? y - bottom : 0
  }
  let bi = 0
  for (let i = 1; i < rows.length; i++) if (dist(rows[i]) < dist(rows[bi])) bi = i
  const row = [...rows[bi]].sort((p, q) => p.left - q.left)
  const slots = row.map((b, i) => ({ x: i === 0 ? b.left : (row[i - 1].right + b.left) / 2, before: b.id as string | null, at: i }))
  slots.push({ x: row[row.length - 1].right, before: rows[bi + 1]?.[0]?.id ?? null, at: row.length })
  let best = slots[0]
  for (const sl of slots) if (Math.abs(sl.x - x) < Math.abs(best.x - x)) best = sl
  const held = prev === undefined ? undefined : slots.find((sl) => sl.before === prev)
  if (held && Math.abs(held.x - x) <= Math.abs(best.x - x) + DROP_STICKY_PX) best = held
  return { before: best.before, shift: row.slice(best.at).map((b) => b.id), after: best.at === row.length ? row[row.length - 1].id : null }
}

export const PIN_GRID_COLS = 4
export const PIN_GRID_ROWS = 3
export const PIN_GRID_MAX = PIN_GRID_COLS * PIN_GRID_ROWS

export function pinGridLayout(count: number, expanded: boolean, rows: number = PIN_GRID_ROWS): { shown: number; more: number; collapsible: boolean } {
  const max = Math.max(1, Math.floor(rows)) * PIN_GRID_COLS
  if (count <= max) return { shown: count, more: 0, collapsible: false }
  if (expanded) return { shown: count, more: 0, collapsible: true }
  const shown = max - 1
  return { shown, more: count - shown, collapsible: false }
}

export function pinGridRowsNeeded(count: number): number {
  return Math.max(1, Math.ceil(count / PIN_GRID_COLS))
}

export function rowsFromDrag(startRows: number, dy: number, rowPx: number, count: number): number {
  const max = pinGridRowsNeeded(count)
  const start = Math.min(Math.max(1, startRows), max)
  if (!(rowPx > 0)) return start
  return Math.min(max, Math.max(1, start + Math.round(dy / rowPx)))
}

const ROWS_KEY = 'agm.pinGridRows'

export function loadPinGridRows(): number {
  try {
    const n = Number(localStorage.getItem(ROWS_KEY))
    return Number.isInteger(n) && n >= 1 && n <= 50 ? n : PIN_GRID_ROWS
  } catch {
    return PIN_GRID_ROWS
  }
}

export function savePinGridRows(n: number): void {
  try {
    localStorage.setItem(ROWS_KEY, String(n))
  } catch {
  }
}

export function endBefore(full: string[], visible: string[]): string | null {
  const last = visible[visible.length - 1]
  const at = last === undefined ? -1 : full.indexOf(last)
  return at < 0 ? null : (full[at + 1] ?? null)
}
