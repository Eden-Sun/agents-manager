/**
 * issue #25：WS 一路 append 的清單要有上限、且不整包 sort（UI 常開一整天）。
 * 超過上限的舊訊息靠 `before=` 分頁補回（API.md §6 / §13.4）。
 */

export const MESSAGE_CAP = 500

export const TURN_CAP = 50

/** 同秒再比 id（ULID 即時間序）。 */
export function byTime(a: { created_at: string; id: string }, b: { created_at: string; id: string }): number {
  const t = a.created_at.localeCompare(b.created_at)
  return t !== 0 ? t : a.id.localeCompare(b.id)
}

/** 群組時間軸只看 id：daemon 的 `before=` 分頁照這個切。 */
export function byId(a: { id: string }, b: { id: string }): number {
  return a.id.localeCompare(b.id)
}

/**
 * 從尾端往前找插入點（接在最後是 O(1)）；重複項排序鍵相同，掃到插入點前必遇到，不必另外去重。
 * 已有同 id 回 `null`（不用動 state）。
 */
export function insertSorted<T extends { id: string }>(
  list: readonly T[],
  item: T,
  cmp: (a: T, b: T) => number,
): T[] | null {
  let at = list.length
  while (at > 0) {
    const prev = list[at - 1]
    if (prev.id === item.id) return null
    if (cmp(prev, item) <= 0) break
    at--
  }
  if (at === list.length) return [...list, item]
  return [...list.slice(0, at), item, ...list.slice(at)]
}

/**
 * 同 `insertSorted`，但同一個 id 已經在清單裡而且 `same` 說不一樣時換掉它（照新內容重新排位置）：daemon 會原地改一則訊息再推一次
 * （遲到的 hook 以原文取代備援抓的回覆，SPEC §4.3）。一樣的就回 `null`，不白重畫。
 */
export function upsertSorted<T extends { id: string }>(
  list: readonly T[],
  item: T,
  cmp: (a: T, b: T) => number,
  same: (a: T, b: T) => boolean,
): T[] | null {
  const at = list.findIndex((x) => x.id === item.id)
  if (at < 0) return insertSorted(list, item, cmp)
  if (same(list[at], item)) return null
  // 時間也可能變了（排過隊的一則送出時改成送出的時間）：拿掉舊的再照順序插回去。
  return insertSorted([...list.slice(0, at), ...list.slice(at + 1)], item, cmp)
}

/** 留最新的；`trimmed` 時呼叫端要打開「還有更早的」旗標。 */
export function capList<T>(list: T[], cap: number): { list: T[]; trimmed: boolean } {
  if (list.length <= cap) return { list, trimmed: false }
  return { list: list.slice(list.length - cap), trimmed: true }
}

/** 刻意不綁 `Turn`，測試才不用造整個物件。 */
export interface Turnish {
  status: string
  delivery?: string | null
}

/**
 * 讀者只有 `inFlightTurn`／`unknownDeliveryTurn`，都只認 in_flight：那些必留，其餘留最近 `cap` 筆
 * （ULID 序）。留一段而非一筆：`loadMessages` 會整頁重灌，砍太乾淨只是再抓一遍。
 */
export function pruneTurns<T extends Turnish>(map: Record<string, T>, cap = TURN_CAP): Record<string, T> {
  const ids = Object.keys(map)
  if (ids.length <= cap) return map
  const keep = new Set(ids.sort((a, b) => a.localeCompare(b)).slice(-cap))
  for (const id of ids) {
    const t = map[id]
    if (t.status === 'in_flight') keep.add(id)
  }
  if (keep.size === ids.length) return map
  const out: Record<string, T> = {}
  for (const id of ids) if (keep.has(id)) out[id] = map[id]
  return out
}


/**
 * 一頁 messages 回來時舊清單留哪些：不能整包換（飛行中收到的 `message_added` 會被蓋掉），
 * 也不能全留（resync 要能刪過期的）。界線是頁內最新一筆，空頁退回 `startedAt`。
 *
 * 界線用 `(created_at, id)` 全序，不只看時間：訊息時間是毫秒，頁抓完之後才 commit 的那一則可能跟頁內最新一則同一毫秒，
 * 只比時間會把它當成「頁裡該有卻沒有的過期項」刪掉（同 #695 的已讀標記）。
 */
export function keptAfterPage<T extends { id: string; created_at: string }>(existing: T[], page: T[], startedAt: string): T[] {
  let newest: T | null = null
  for (const m of page) if (!newest || byTime(m, newest) > 0) newest = m
  const seen = new Set(page.map((m) => m.id))
  return existing.filter((m) => !seen.has(m.id) && (newest ? byTime(m, newest) > 0 : m.created_at > startedAt))
}
