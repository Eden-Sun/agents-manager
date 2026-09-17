/**
 * 回合「已完成（未讀）」的前端帳本（daemon 不知情）：純函式 + localStorage。
 * 數字與已讀標記都存：標記是真相，但重整後只有正在看的 bot 有訊息可重算，其他靠存下的數字。
 */

import type { Message } from '../api/types'
import { writeShared } from './mobilePreview'

const MARKS_KEY = 'am.readMarks'
const COUNTS_KEY = 'am.unread'

/** bot 對話或 project 群組聊天（§13）。 */
export type UnreadKey = `bot:${string}` | `group:${string}`

export const botKey = (botId: string): UnreadKey => `bot:${botId}`
export const groupKey = (projectId: string): UnreadKey => `group:${projectId}`

/** 時間是主要依據，id 只用來排除標記那一則本身。 */
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
    writeShared(() => localStorage.setItem(MARKS_KEY, JSON.stringify(marks)))
  } catch {
    /* 無痕／禁用儲存：只是不跨重整 */
  }
}

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
    writeShared(() => localStorage.setItem(COUNTS_KEY, JSON.stringify(flat)))
  } catch {
    /* 同上 */
  }
}

/**
 * 分頁可見且視窗有 focus（選取對話由呼叫端判斷）。少了 focus，瀏覽器丟旁邊螢幕、人在別的 app
 * 時進來的回覆會被當已讀——那正是最需要徽章的情境。
 */
export function windowActive(): boolean {
  if (typeof document === 'undefined') return true
  if (document.visibilityState !== 'visible') return false
  return typeof document.hasFocus === 'function' ? document.hasFocus() : true
}

export function completesTurn(msg: Pick<Message, 'role'>): boolean {
  return msg.role === 'assistant'
}

/** 沒有標記 = 什麼都沒讀過。 */
export function isUnread(msg: Pick<Message, 'id' | 'created_at'>, mark: ReadMark | undefined): boolean {
  if (!mark) return true
  if (msg.created_at > mark.at) return true
  // daemon 時間是秒級、常撞：同時間戳只有標記那則算已讀。
  return msg.created_at === mark.at && msg.id !== mark.id
}

/** 以回合計數：同回合多則 assistant 訊息算一個；沒 turn_id 的各算一個。 */
export function countUnreadTurns(messages: readonly Message[], mark: ReadMark | undefined): number {
  const turns = new Set<string>()
  for (const m of messages) {
    if (!completesTurn(m) || !isUnread(m, mark)) continue
    turns.add(m.turn_id ?? `msg:${m.id}`)
  }
  return turns.size
}

/**
 * 必須與 `turn_updated` 用同一個 key（turn.id）才能去重。沒 turn_id（防禦路徑）就掛到最近的
 * 回合（ULID 字典序＝時間序）；連回合都不知道才退回 `msg:<id>`，徽章不能因此不亮。
 */
export function completionKey(msg: Pick<Message, 'id' | 'turn_id'>, knownTurnIds: readonly string[]): string {
  if (msg.turn_id) return msg.turn_id
  let latest: string | null = null
  for (const id of knownTurnIds) if (!latest || id > latest) latest = id
  return latest ?? `msg:${msg.id}`
}

export function markOfMessages(messages: readonly Message[]): ReadMark | null {
  let best: ReadMark | null = null
  for (const m of messages) {
    if (!best || m.created_at > best.at) best = { at: m.created_at, id: m.id }
  }
  return best
}

export function markNow(): ReadMark {
  return { at: new Date().toISOString(), id: '' }
}

/** 不清的話，刪掉的 bot 會在分頁標題 `(N)` 留下永遠點不掉的未讀。 */
export function pruneUnread<T>(book: Record<string, T>, live: (id: string) => boolean): Record<string, T> {
  const out: Record<string, T> = {}
  for (const [k, v] of Object.entries(book)) if (live(k)) out[k] = v
  return out
}

/** 同上，給 `bot:`／`group:` 前綴的標記表。 */
export function pruneMarks(marks: Record<string, ReadMark>, liveBot: (id: string) => boolean, liveProject: (id: string) => boolean) {
  return pruneUnread(marks, (k) =>
    k.startsWith('bot:') ? liveBot(k.slice(4)) : k.startsWith('group:') ? liveProject(k.slice(6)) : false,
  )
}

/**
 * 分頁標題 `(N)` 只加 bot：群組未讀是同一批回覆，相加會一則算兩次。
 * 側欄收起來的那些不算：使用者看到 `(7)` 卻在側欄數不出七個未讀，只會以為數字壞了。
 */
export function totalUnread(bots: Record<string, number>, hidden: readonly string[] = []): number {
  const skip = new Set(hidden)
  let n = 0
  for (const [id, v] of Object.entries(bots)) if (!skip.has(id)) n += v
  return n
}

/**
 * 側欄顯不顯示這顆 bot 的未讀：總管專案裡的往來是 AGM 的內部事務，不顯示（2026-09-16 使用者）。
 * 側欄與分頁標題共用這一條——標題照算的話，AGM 例行回合讓 `(N)` 長期掛著，側欄卻找不到一筆點得掉（review3 c5 L2）。
 */
export function unreadShown(bot: { project_id: string } | undefined, supervisorProjectId: string | null): boolean {
  return !(bot && supervisorProjectId && bot.project_id === supervisorProjectId)
}

/** 分頁標題的 `(N)`：側欄收起來的與總管專案的都不算。 */
export function titleUnread(s: {
  botUnread: Record<string, number>
  hiddenBotIds: readonly string[]
  bots: readonly { id: string; project_id: string }[]
  supervisorProjectId: string | null
}): number {
  const agm = s.bots.filter((b) => !unreadShown(b, s.supervisorProjectId)).map((b) => b.id)
  return totalUnread(s.botUnread, agm.length === 0 ? s.hiddenBotIds : [...s.hiddenBotIds, ...agm])
}

/** `message_added` 與 `turn_updated` 都代表完成，去重避免一回合跳兩下；只留最近 500 筆。 */
const counted = new Set<string>()
const COUNTED_CAP = 500

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

/** 測試用。 */
export function resetTurnCompletions() {
  counted.clear()
}

/**
 * `bot_status` working→idle 補記的 key（終端直接輸入或沒裝 hook；docs/reviews/2026-09-12/web.md §2）。
 * 不能沿用最大 turn id：那筆多半已被 hook 記過，網頁送過一次後終端回合就永遠不亮。
 * in_flight → 共用 turn id 去重；hook 剛記過 → null（是它的尾巴）；否則 `run:<id>:<第幾次 idle>`。
 */
const hookCompleted = new Set<string>()
const idleEdges = new Map<string, number>()

/** 接下來的那個 idle 邊緣屬於這次 hook 記的完成。 */
export function markHookCompletion(botId: string) {
  hookCompleted.add(botId)
}

/** idle→working：清掉上一輪的標記。 */
export function clearHookCompletion(botId: string) {
  hookCompleted.delete(botId)
}

export function idleEdgeCompletionKey(
  botId: string,
  runId: string,
  latestTurn: { id: string; status: string } | null,
): string | null {
  if (latestTurn && latestTurn.status === 'in_flight') return latestTurn.id
  if (hookCompleted.delete(botId)) return null
  const n = (idleEdges.get(botId) ?? 0) + 1
  idleEdges.set(botId, n)
  return `run:${runId}:${n}`
}

/** 測試用。 */
export function resetIdleEdges() {
  hookCompleted.clear()
  idleEdges.clear()
}
