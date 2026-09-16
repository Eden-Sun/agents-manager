import type { Bot } from '../api/types'
import type { ReadMark } from './unread'

/**
 * 跨裝置已讀（2026-09-15 使用者：「手機桌機主力區不一樣，少了完成的訊息」）。本機 localStorage 的標記與 daemon 的
 * 共用標記取較新的那個；未讀數以 daemon 算的為準——手機分頁被凍結時收不到「回合完成」，本機的 +1 永遠補不回來。
 */
export function laterMark(a: ReadMark | undefined | null, b: ReadMark | undefined | null): ReadMark | undefined {
  if (!a) return b ?? undefined
  if (!b) return a
  if (a.at !== b.at) return a.at > b.at ? a : b
  return a.id >= b.id ? a : b
}

/**
 * daemon 有給數字的 bot 才覆寫；正在看的那顆不動（它的已讀正在送出，快照可能還是舊的）。
 * `unsent` 是本機已讀但 `POST /api/bots/:id/read` 還沒送到的 bot：daemon 的數字對它們來說是舊的，
 * 照抄會把使用者剛清掉的徽章點回來（daemon 重啟時很容易發生），補送成功前一律跳過。
 */
export function serverUnread(
  bots: readonly Bot[],
  current: Record<string, number>,
  viewing: (id: string) => boolean,
  unsent: { has: (id: string) => boolean } = new Set<string>(),
): Record<string, number> | null {
  let changed = false
  const next = { ...current }
  for (const b of bots) {
    if (b.pending || typeof b.unread !== 'number' || viewing(b.id) || unsent.has(b.id)) continue
    const have = current[b.id] ?? 0
    if (have === b.unread) continue
    changed = true
    if (b.unread > 0) next[b.id] = b.unread
    else delete next[b.id]
  }
  return changed ? next : null
}
