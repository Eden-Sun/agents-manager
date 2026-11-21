export interface ChipBox {
  pinned: boolean
  top: number
  bottom?: number
}

export function lineBudget(hasPinned: boolean): { pinned: number; others: number } {
  return { pinned: 3, others: hasPinned ? 1 : 2 }
}

export function clipAfterRows(
  boxes: { top: number; bottom?: number }[],
  budget: number,
): { hidden: number[]; visibleBottom: number } {
  const rows = [...new Set(boxes.map((b) => b.top))].sort((a, b) => a - b)
  if (rows.length <= budget) return { hidden: [], visibleBottom: 0 }
  const last = rows[budget - 1]
  const hidden: number[] = []
  let visibleBottom = 0
  boxes.forEach((b, i) => {
    if (b.top > last) hidden.push(i)
    else visibleBottom = Math.max(visibleBottom, b.bottom ?? b.top)
  })
  return { hidden, visibleBottom }
}

export interface LayoutBox {
  offsetTop: number
  offsetHeight: number
}

export function layoutBoxes(chips: LayoutBox[], groupTop: number): { top: number; bottom: number }[] {
  return chips.map((c) => ({ top: c.offsetTop - groupTop, bottom: c.offsetTop - groupTop + c.offsetHeight }))
}

export function orderChanged(prev: string[], next: string[]): boolean {
  const common = new Set(prev.filter((id) => next.includes(id)))
  const a = prev.filter((id) => common.has(id))
  const b = next.filter((id) => common.has(id))
  return a.some((id, i) => id !== b[i])
}

export function hiddenChipIndexes(boxes: ChipBox[]): number[] {
  const budget = lineBudget(boxes.some((b) => b.pinned))
  const out: number[] = []
  for (const pinned of [true, false]) {
    const idx = boxes.map((b, i) => ({ b, i })).filter((x) => x.b.pinned === pinned)
    for (const at of clipAfterRows(idx.map((x) => x.b), pinned ? budget.pinned : budget.others).hidden) out.push(idx[at].i)
  }
  return out.sort((a, b) => a - b)
}

import { t } from './i18n'

export function moreTitle(hidden: { needsReply: boolean; unread: number }[]): string {
  const ask = hidden.filter((h) => h.needsReply).length
  const unread = hidden.filter((h) => !h.needsReply && h.unread > 0).length
  const parts = [
    ask > 0 ? t('{n} 顆要你回答', { n: ask }) : '',
    unread > 0 ? t('{n} 顆有未讀', { n: unread }) : '',
  ].filter(Boolean)
  const what = parts.length > 0 ? t('其中 {items}', { items: parts.join(t('、')) }) : t('都是比較不急的')
  return t('還有 {n} 顆沒顯示（{what}）。點一下展開', { n: hidden.length, what })
}
