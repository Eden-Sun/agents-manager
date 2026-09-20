/**
 * 主力那列（★）的固定順序與拖曳計算（issue #344）。順序存 daemon 的 `primary_position`，
 * 不再照未讀／忙碌的優先序跳位；這裡只放純函式，事件處理在 `components/usePinnedDrag.ts`。
 */

export interface Positioned {
  id: string
  /** daemon 的 `primary_position`；沒有＝0。 */
  position: number
  /** 原本的清單順序，同位置時的穩定決勝。 */
  index: number
}

/** 主力組排序：`primary_position` 小的在前，同值照原順序（未拖過時 daemon 全給 0＝維持側欄順序）。 */
export function sortPinned<T extends Positioned>(items: T[]): T[] {
  return [...items].sort((a, b) => a.position - b.position || a.index - b.index)
}

/** 把 `id` 移到 `beforeId` 前面（null＝最後）；沒有變動回 null。 */
export function moveBefore(order: string[], id: string, beforeId: string | null): string[] | null {
  if (id === beforeId || !order.includes(id)) return null
  const rest = order.filter((x) => x !== id)
  const at = beforeId === null ? rest.length : rest.indexOf(beforeId)
  if (beforeId !== null && at < 0) return null
  const next = [...rest.slice(0, at), id, ...rest.slice(at)]
  return next.join('\n') === order.join('\n') ? null : next
}

/** 鍵盤：往左／右一格。撞到頭尾回 null。 */
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

/**
 * 放開位置對應的「插在誰前面」。晶片會換行，所以不只看 x：先找同一行（y 落在該列的上下界，找不到就挑垂直距離最近的一行），
 * 再在那一行裡找第一顆中心點在游標右邊的——它就是 `beforeId`，都沒有就是接在這行最後一顆之後
 * （＝下一行第一顆之前，全列最後則 null）。拖的那一顆自己不參與比較。
 */
export function dropBefore(boxes: Box[], x: number, y: number, dragId: string): string | null {
  const others = boxes.filter((b) => b.id !== dragId)
  if (others.length === 0) return null
  // 依 top 分行。
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
  const hit = row.find((b) => (b.left + b.right) / 2 > x)
  if (hit) return hit.id
  return rows[bi + 1]?.[0]?.id ?? null
}
