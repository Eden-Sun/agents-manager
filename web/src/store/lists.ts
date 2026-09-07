/**
 * issue #25：store 裡由 WS 一路 append 的清單（`messages` / `groupMessages` /
 * `turns` / `teamEvents`）原本沒有上限，也沒有淘汰；每則 `message_added` 還要對整包做
 * 一次 `sort`。daemon 是常駐的、UI 一開就是一整天，這兩件事都會隨時間線性變貴。
 *
 * 這裡放兩件事：
 *
 * 1. **插入定位取代整包排序**（`insertSorted`）。清單本來就是排好序的，新的一則幾乎永遠
 *    落在尾端，所以從尾巴往前找插入點：常見情形只比一次就 `push`，取代原本每則
 *    `sortByTime([...existing, msg])` 的 O(n log n) 複製 + 排序。
 * 2. **上限**（`capList`）。超過就從頭截掉，更舊的靠「載入更早的訊息」（`before=` 分頁）
 *    回來——daemon 那頭本來就支援（API.md §6 / §13.4），只是前端從來沒有用過。
 */

/** 每個 bot / project 的訊息清單上限。超過就截掉最舊的，改由分頁補回來。 */
export const MESSAGE_CAP = 500

/** 每個 team 的事件清單上限。時間軸只有最近的有人在看，舊的沒有分頁可補，就純粹丟掉。 */
export const TEAM_EVENT_CAP = 500

/** 一個 bot 底下保留的 Turn 數上限，見 `pruneTurns`。 */
export const TURN_CAP = 50

/** 對話順序：`created_at`，同秒再比 id（ULID 本身就是時間序）。 */
export function byTime(a: { created_at: string; id: string }, b: { created_at: string; id: string }): number {
  const t = a.created_at.localeCompare(b.created_at)
  return t !== 0 ? t : a.id.localeCompare(b.id)
}

/** 群組時間軸順序：只看 id（ULID）——daemon 的 `before=` 分頁也是照這個切的。 */
export function byId(a: { id: string }, b: { id: string }): number {
  return a.id.localeCompare(b.id)
}

/**
 * 把 `item` 放進已排序的 `list`，回傳新陣列；`list` 本身不動。
 *
 * 從尾端往前線性找插入點，所以「新的一則接在最後」是 O(1)。因為 `list` 有序、而重複的
 * 那則排序鍵一定跟新的相同，掃到插入點就一定會遇到它——不用再多掃一次整包去重。
 * 已經有同 id 的就回 `null`（＝這個 frame 不用動 state，維持原本 `some(...)` 的語意）。
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
 * 把清單截到 `cap` 則（留最新的）。`trimmed` 為真表示前面真的少了東西，呼叫端要把
 * 「還有更早的」旗標打開，使用者才點得到「載入更早的訊息」。
 */
export function capList<T>(list: T[], cap: number): { list: T[]; trimmed: boolean } {
  if (list.length <= cap) return { list, trimmed: false }
  return { list: list.slice(list.length - cap), trimmed: true }
}

/** `pruneTurns` 只看得到這兩個欄位——刻意不綁 `Turn`，測試才不用造整個物件。 */
export interface Turnish {
  status: string
  delivery?: string | null
}

/**
 * 一個 bot 的 Turn map 只有兩種讀者：`inFlightTurn`（進行中的那筆）與
 * `unknownDeliveryTurn`（送出狀態不明、擋著下一則 prompt 的那筆）。回合結束之後那筆
 * 就沒有人會再讀，卻永遠留在 map 裡——沒選到的 bot 更是只增不減。
 *
 * 所以：`in_flight` 與 `delivery === 'unknown'`（且沒 failed）的一定留，其餘只留最近
 * `TURN_CAP` 筆（id 是 ULID，字典序就是時間序）。留一小段而不是只留一筆，是因為
 * `loadMessages` 會用整頁的 turn 重灌這個 map，砍太乾淨只是讓它下次再抓一遍。
 */
export function pruneTurns<T extends Turnish>(map: Record<string, T>, cap = TURN_CAP): Record<string, T> {
  const ids = Object.keys(map)
  if (ids.length <= cap) return map
  const keep = new Set(ids.sort((a, b) => a.localeCompare(b)).slice(-cap))
  for (const id of ids) {
    const t = map[id]
    if (t.status === 'in_flight' || (t.delivery === 'unknown' && t.status !== 'failed')) keep.add(id)
  }
  if (keep.size === ids.length) return map
  const out: Record<string, T> = {}
  for (const id of ids) if (keep.has(id)) out[id] = map[id]
  return out
}

