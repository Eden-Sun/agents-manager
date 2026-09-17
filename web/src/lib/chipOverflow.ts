/**
 * 桌機晶片列收合時哪幾顆藏進 `+N`（`UnreadChip.tsx`）。
 *
 * ★ 主力一組、其餘一組，各自換行（2026-09-15 使用者）。兩組都有東西時**各佔一行**，只有一組時那組最多兩行。
 * 以前是整列裁兩行：主力多到占滿兩行時，沒釘的「要你回答」被擠到第三行、收進 `+N`，提示還說「都是比較不急的」
 * ——跟 2026-09-12 錯過 blocked bot 的事故同形（review3 c4 L2）。組內已照緊急度排好，所以每組藏掉的一定是
 * 該組最不急的；兩組之間不比，提示就照實際藏起來的寫。
 */
export interface ChipBox {
  pinned: boolean
  /** 組內的上緣（同一行的晶片同值）。 */
  top: number
  /** 組內的下緣；算裁切高度用，不給就當跟 `top` 同高。 */
  bottom?: number
}

/** 每一組收合時能佔幾行。 */
export function lineBudget(hasPinned: boolean, hasOthers: boolean): number {
  return hasPinned && hasOthers ? 1 : 2
}

/**
 * 一組裡：`budget` 行之後被裁掉的晶片（索引），以及看得到的最後一行的下緣。
 * 裁切高度是量出來的，不寫死 px——晶片加了星號與徽章就會變高，寫死會把最後一行切一半（541afe7）。
 */
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

/** 收合時會被藏起來的晶片（回傳索引）：兩組各自算，組內規則同 [`clipAfterRows`]。 */
export function hiddenChipIndexes(boxes: ChipBox[]): number[] {
  const budget = lineBudget(boxes.some((b) => b.pinned), boxes.some((b) => !b.pinned))
  const out: number[] = []
  for (const pinned of [true, false]) {
    const idx = boxes.map((b, i) => ({ b, i })).filter((x) => x.b.pinned === pinned)
    for (const at of clipAfterRows(idx.map((x) => x.b), budget).hidden) out.push(idx[at].i)
  }
  return out.sort((a, b) => a - b)
}

/** `+N` 的提示：藏起來的有要回答／未讀就明說，全是不急的才說不急。 */
export function moreTitle(hidden: { needsReply: boolean; unread: number }[]): string {
  const ask = hidden.filter((h) => h.needsReply).length
  const unread = hidden.filter((h) => !h.needsReply && h.unread > 0).length
  const parts = [ask > 0 ? `${ask} 顆要你回答` : '', unread > 0 ? `${unread} 顆有未讀` : ''].filter(Boolean)
  const what = parts.length > 0 ? `其中 ${parts.join('、')}` : '都是比較不急的'
  return `還有 ${hidden.length} 顆沒顯示（${what}）。點一下展開`
}
