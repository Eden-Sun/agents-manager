import type { Message } from '../api/types'

/**
 * 專案標題的藍色數字只算「群組回覆」（2026-09-15 使用者選 1）：以前專案裡任何 bot 完成回合都 +1，
 * Agents Manager 這種 bot 多的專案一下就 99+，跟說明「未讀的群組回覆」對不上。
 *
 * 群組回覆＝回的是群組訊息的那一回合：使用者在群組發言時，每顆收件 bot 的對話會多一則帶 `group_id` 的
 * user 訊息（同 `turn_id`）；bot 的回覆本身 `group_id` 是 null（API.md §11.1）。所以記下這些 turn，完成時才算。
 */
const groupTurns = new Set<string>()
const MAX_TRACKED = 500

export function noteGroupPrompt(msg: Pick<Message, 'role' | 'group_id' | 'turn_id'>) {
  if (msg.role !== 'user' || !msg.group_id || !msg.turn_id) return
  groupTurns.add(msg.turn_id)
  // Set 依插入順序：超量就丟最舊的。
  while (groupTurns.size > MAX_TRACKED) {
    const oldest = groupTurns.values().next().value
    if (oldest === undefined) break
    groupTurns.delete(oldest)
  }
}

/** 已載入的群組時間軸也是來源：重整後還在跑的群組回合照樣能被認出來。 */
export function noteGroupPrompts(messages: readonly Pick<Message, 'role' | 'group_id' | 'turn_id'>[]) {
  for (const m of messages) noteGroupPrompt(m)
}

export function isGroupTurn(turnId: string | null | undefined): boolean {
  return Boolean(turnId && groupTurns.has(turnId))
}

const RESET_KEY = 'am.groupUnread.v2'

/** 舊算法累積的數字（常是 99+）升級後清一次，之後照新規則算。 */
export function dropLegacyGroupCounts<T>(groups: Record<string, T>): Record<string, T> {
  try {
    if (localStorage.getItem(RESET_KEY)) return groups
    localStorage.setItem(RESET_KEY, '1')
    return {}
  } catch {
    return groups
  }
}

export function resetGroupTurnsForTest() {
  groupTurns.clear()
}
