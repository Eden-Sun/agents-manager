/**
 * issue #25：WS 一路 append 的清單要有上限、且不整包 sort（UI 常開一整天）。
 * 超過上限的舊訊息靠 `before=` 分頁補回（API.md §6 / §13.4）。
 */

export const MESSAGE_CAP = 500

export const TURN_CAP = 50

/**
 * 兩邊都有 `seq`（daemon 的 rowid，單調的插入序）且不同就照它；任一邊沒有（舊 daemon、樂觀訊息）或是 0＝未知，
 * 退回 id（ULID）。同一份資料不會一半有一半沒有，所以這個混合比較在實務上仍是全序。
 */
function bySeqThenId(a: { id: string; seq?: number }, b: { id: string; seq?: number }): number {
  if (a.seq && b.seq && a.seq !== b.seq) return a.seq < b.seq ? -1 : 1
  return a.id.localeCompare(b.id)
}

/** 先比時間（毫秒）；同毫秒用 `seq`——ULID 的隨機段在同一毫秒內不單調，舊資料的同毫秒訊息 id 序不是插入序。 */
export function byTime(a: { created_at: string; id: string; seq?: number }, b: { created_at: string; id: string; seq?: number }): number {
  const t = a.created_at.localeCompare(b.created_at)
  return t !== 0 ? t : bySeqThenId(a, b)
}

/** 群組時間軸看插入序：daemon 的 `before=` 分頁照 rowid 切（`seq` 就是它）；沒有 seq 才退回 id。 */
export function byInsert(a: { id: string; seq?: number }, b: { id: string; seq?: number }): number {
  return bySeqThenId(a, b)
}

/** 最早插入的一則：before 游標依 rowid 切頁，不一定是依 created_at 顯示時排在最前的那則。 */
export function oldestByInsert<T extends { id: string; seq?: number }>(items: readonly T[]): T | undefined {
  let oldest: T | undefined
  for (const item of items) if (!oldest || byInsert(item, oldest) < 0) oldest = item
  return oldest
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
 * 界線用 `(created_at, seq)`（沒有 seq 退回 id）全序，不只看時間：訊息時間是毫秒，頁抓完之後才 commit 的那一則可能跟頁內最新一則同一毫秒，
 * 只比時間會把它當成「頁裡該有卻沒有的過期項」刪掉（同 #695 的已讀標記）。
 */
export function keptAfterPage<T extends { id: string; created_at: string; seq?: number }>(existing: T[], page: T[], startedAt: string): T[] {
  let newest: T | null = null
  for (const m of page) if (!newest || byTime(m, newest) > 0) newest = m
  const seen = new Set(page.map((m) => m.id))
  return existing.filter((m) => !seen.has(m.id) && (newest ? byTime(m, newest) > 0 : m.created_at > startedAt))
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false
  if (Array.isArray(a) !== Array.isArray(b)) return false
  const ka = Object.keys(a)
  if (ka.length !== Object.keys(b).length) return false
  for (const k of ka) {
    if (!deepEqual((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k])) return false
  }
  return true
}

/**
 * 一頁重載回來的清單裡，內容沒變的項目沿用舊物件。`loadMessages`／`loadGroupMessages` 每次都把整頁重新 normalize 成
 * 新物件（WS 重連、resync、送失敗重讀、撤回都會走），`memo(Bubble)` 看到每則的 `msg` 都換了身分，整段歷史重新渲染——
 * 每則都重新解析 markdown（量測：500 則短訊息約 1 秒、每 KB 約 11 ms）。內容一樣就留舊的；全部一樣（同順序）時連陣列也回舊的，
 * 訂閱整份清單的選取器才不會白跑。內容變了的（遲到的 hook 以原文取代備援抓的回覆）才換新物件。
 */
export function reuseUnchanged<T extends { id: string }>(prev: readonly T[], next: T[]): T[] {
  if (prev.length === 0) return next
  const old = new Map<string, T>()
  for (const x of prev) old.set(x.id, x)
  let same = prev.length === next.length
  const out = next.map((n, i) => {
    const o = old.get(n.id)
    if (o !== undefined && deepEqual(o, n)) {
      if (prev[i] !== o) same = false
      return o
    }
    same = false
    return n
  })
  return same ? (prev as T[]) : out
}
