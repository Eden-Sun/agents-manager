/**
 * 「已完成（未讀）」——回合狀態的中間段。
 *
 * 一個回合跑完的當下，使用者不一定在看：他可能開著別的 bot、把分頁切走、或整個視窗都沒
 * focus。那一刻的「完成」對他來說還沒發生，要等他真的看到。所以回合的狀態是
 * 進行中 → 已完成（未讀）→ 已完成（已讀），中間那段就記在這裡。
 *
 * 這一份是純函式 + localStorage，跟 store 只透過幾個小 hook 相接（見 store.ts 的
 * `botUnread` / `markBotRead` / `recountBot`）：未讀是前端自己的帳，daemon 完全不知情。
 *
 * 為什麼「數字」和「已讀標記」兩個都存：
 * - 標記（最後已讀的訊息 id + 時間）是**真相**，但要拿它算出數字得有那個 bot 的訊息，
 *   而重整之後只有正在看的那個 bot 會載入訊息，其他的一則都沒有。
 * - 所以數字也一起存；訊息真的載進來時再用標記重算一次（`countUnreadTurns`）校正。
 */

import type { Message } from '../api/types'

const MARKS_KEY = 'am.readMarks'
const COUNTS_KEY = 'am.unread'

/** 未讀帳本的 key：一個 bot 的對話，或一個 project 的群組聊天（§13）。 */
export type UnreadKey = `bot:${string}` | `group:${string}`

export const botKey = (botId: string): UnreadKey => `bot:${botId}`
export const groupKey = (projectId: string): UnreadKey => `group:${projectId}`

/** 某個對話最後被讀到哪裡。時間是主要依據，id 只用來排除「剛好就是標記那一則」。 */
export interface ReadMark {
  at: string
  id: string
}

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

export function loadMarks(): Record<string, ReadMark> {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(MARKS_KEY) ?? 'null')
    if (!isRec(parsed)) return {}
    const out: Record<string, ReadMark> = {}
    for (const [k, v] of Object.entries(parsed)) {
      if (!isRec(v)) continue
      const at = v.at
      const id = v.id
      if (typeof at !== 'string' || !at) continue
      out[k] = { at, id: typeof id === 'string' ? id : '' }
    }
    return out
  } catch {
    return {}
  }
}

export function saveMarks(marks: Record<string, ReadMark>) {
  try {
    localStorage.setItem(MARKS_KEY, JSON.stringify(marks))
  } catch {
    /* 無痕視窗 / 關掉儲存：未讀在這一頁仍然正確，只是不跨重整 */
  }
}

/** 存下來的未讀數，拆成 bot 與 group 兩張表（store 裡本來就是兩個欄位）。 */
export interface UnreadCounts {
  bots: Record<string, number>
  groups: Record<string, number>
}

export function loadCounts(): UnreadCounts {
  const out: UnreadCounts = { bots: {}, groups: {} }
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(COUNTS_KEY) ?? 'null')
    if (!isRec(parsed)) return out
    for (const [k, v] of Object.entries(parsed)) {
      const n = typeof v === 'number' && Number.isFinite(v) ? Math.floor(v) : 0
      if (n <= 0) continue
      if (k.startsWith('bot:')) out.bots[k.slice(4)] = n
      else if (k.startsWith('group:')) out.groups[k.slice(6)] = n
    }
  } catch {
    /* 讀不到就當作全部已讀 */
  }
  return out
}

export function saveCounts(counts: UnreadCounts) {
  const flat: Record<string, number> = {}
  for (const [id, n] of Object.entries(counts.bots)) if (n > 0) flat[botKey(id)] = n
  for (const [id, n] of Object.entries(counts.groups)) if (n > 0) flat[groupKey(id)] = n
  try {
    localStorage.setItem(COUNTS_KEY, JSON.stringify(flat))
  } catch {
    /* 同上 */
  }
}

/**
 * 使用者現在真的看得到畫面嗎。
 *
 * 三件事都要成立才算「在看」：分頁可見、視窗有 focus、而且（呼叫端判斷）選取的正好是這個
 * 對話。少了 focus 這一項，把瀏覽器丟在旁邊螢幕、人在別的 app 打字時進來的回覆會被當成
 * 已讀——那正是最需要留著徽章的情境。
 */
export function windowActive(): boolean {
  if (typeof document === 'undefined') return true
  if (document.visibilityState !== 'visible') return false
  return typeof document.hasFocus === 'function' ? document.hasFocus() : true
}

/** 這則訊息代表「一個回合完成了」嗎。使用者自己送出的、系統註記都不算。 */
export function completesTurn(msg: Pick<Message, 'role'>): boolean {
  return msg.role === 'assistant'
}

/** 這則訊息在已讀標記之後（＝還沒讀過）嗎。沒有標記 = 什麼都沒讀過。 */
export function isUnread(msg: Pick<Message, 'id' | 'created_at'>, mark: ReadMark | undefined): boolean {
  if (!mark) return true
  if (msg.created_at > mark.at) return true
  // 同一個時間戳的其他訊息（daemon 的秒級時間常常撞在一起）：標記那一則自己已讀，其餘算未讀。
  return msg.created_at === mark.at && msg.id !== mark.id
}

/**
 * 從整串訊息算出未讀的**回合**數。同一個回合可能有多則 assistant 訊息（續寫、工具輸出），
 * 那是一個回合、一個數字；沒有 turn_id 的就各自算一個。
 */
export function countUnreadTurns(messages: readonly Message[], mark: ReadMark | undefined): number {
  const turns = new Set<string>()
  for (const m of messages) {
    if (!completesTurn(m) || !isUnread(m, mark)) continue
    turns.add(m.turn_id ?? `msg:${m.id}`)
  }
  return turns.size
}

/** 這串訊息讀完之後的已讀標記（最後一則）。空的話回 `null`。 */
export function markOfMessages(messages: readonly Message[]): ReadMark | null {
  let best: ReadMark | null = null
  for (const m of messages) {
    if (!best || m.created_at > best.at) best = { at: m.created_at, id: m.id }
  }
  return best
}

/** 沒有訊息可依據時的標記：現在。之後進來的一律算未讀，之前的一律算已讀。 */
export function markNow(): ReadMark {
  return { at: new Date().toISOString(), id: '' }
}

/**
 * 丟掉已經不存在的 bot / project 的帳。不清的話 localStorage 會一直長，而且刪掉的 bot
 * 留下的數字會永遠加在分頁標題的 `(N)` 上——那是一個永遠點不掉的未讀。
 */
export function pruneUnread<T>(book: Record<string, T>, live: (id: string) => boolean): Record<string, T> {
  const out: Record<string, T> = {}
  for (const [k, v] of Object.entries(book)) if (live(k)) out[k] = v
  return out
}

/** 同上，但吃的是 `bot:` / `group:` 前綴的 key（已讀標記那張表）。 */
export function pruneMarks(marks: Record<string, ReadMark>, liveBot: (id: string) => boolean, liveProject: (id: string) => boolean) {
  return pruneUnread(marks, (k) =>
    k.startsWith('bot:') ? liveBot(k.slice(4)) : k.startsWith('group:') ? liveProject(k.slice(6)) : false,
  )
}

/**
 * 分頁標題的 `(N)` 用的總數——只加 bot 那一邊。
 *
 * 群組的未讀跟 bot 的是**同一批回覆**（每則 assistant 訊息同時記在它自己的 bot 和它專案的
 * 群組聊天上），兩邊相加會把一個回合數成兩個：一則回覆、標題卻寫 `(2)`。
 */
export function totalUnread(bots: Record<string, number>): number {
  let n = 0
  for (const v of Object.values(bots)) n += v
  return n
}

/**
 * 同一個回合可能先送 `message_added`（assistant）再送 `turn_updated`（終態），兩個都是
 * 「完成」。記住已經算過的回合，才不會一個回合跳兩下。只留最近 500 筆——舊的回合不會再回來。
 */
const counted = new Set<string>()
const COUNTED_CAP = 500

/** 第一次看到這個回合的完成 = `true`（並記下來）；重複的 = `false`。 */
export function takeTurnCompletion(botId: string, turnId: string): boolean {
  const key = `${botId}:${turnId}`
  if (counted.has(key)) return false
  if (counted.size >= COUNTED_CAP) {
    const oldest = counted.values().next().value
    if (oldest !== undefined) counted.delete(oldest)
  }
  counted.add(key)
  return true
}

/** 測試用：清掉回合去重表。 */
export function resetTurnCompletions() {
  counted.clear()
}
