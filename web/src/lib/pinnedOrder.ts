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

/** 換手的遲滯（px）：游標離目前鎖定的落點不比離新落點遠超過這個距離，就維持原落點——拖到附近就鎖定、不在兩個落點的交界抖動。 */
export const DROP_STICKY_PX = 16

export interface Slot {
  /** 插在誰前面；null＝全列最後。 */
  before: string | null
  /** 落點讓位時要往右推的晶片（同一行、落點之後的）。 */
  shift: string[]
  /** 落點在這一行的行尾時，那一行的最後一顆（落點標示畫在它右邊）；否則 null。 */
  after: string | null
}

/**
 * 放開位置對應的落點（#344 第二輪：使用者「drop 點放開大一點」）。跟 `dropBefore` 同一套分行，但改成**最近的插入縫隙**：
 * 每顆晶片之間（與行首、行尾）各一個插入點，游標選離它最近的一個，命中區從縫隙往兩邊各延伸到相鄰晶片的中線（不要求對準縫隙）；
 * 再加遲滯（`prev`＝目前鎖定的落點）。`boxes` 要是**沒被落點讓位推過**的位置（呼叫端把讓位的位移扣掉），不然讓位一開、量到的位置一動就會來回抖。
 */
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
