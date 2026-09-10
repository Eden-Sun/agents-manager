/**
 * 標題列上的「有幾顆母 bot 做完了、還沒看」。
 *
 * 側欄本來就會在每一列上亮未讀（`Sidebar` 的 `.unread-turns`），但側欄在手機上是收起來的、
 * 在桌面上也可能被捲到看不見的位置。使用者真正想追的是「我丟出去的那幾件事，哪些回來了」，
 * 那是一個跨 bot 的問題，所以它要待在每個畫面都看得到的地方——標題列。
 *
 * 只算**母 bot**（`parent_bot_id === null`）：herdr 開出來的子 agent 是母 bot 自己的工人，
 * 它們回話是給母 bot 看的，算進來只會讓數字比使用者實際要處理的事情多。
 */
import { useMemo } from 'react'
import { useStore } from '../store/store'
import './unreadChip.css'

export function UnreadChip() {
  // 兩個欄位分開選、在 `useMemo` 裡才組成陣列：selector 每次回一個新陣列會讓 zustand
  // 每一幀都判定「變了」，畫面就停不下來。
  const bots = useStore((s) => s.bots)
  const botUnread = useStore((s) => s.botUnread)
  // 只算母 bot（`parent_bot_id === null`）：herdr 開出來的子 agent 是母 bot 自己的工人，
  // 它們回話是給母 bot 看的，算進來只會讓數字比使用者實際要處理的事情多。
  const rows = useMemo(
    () =>
      bots
        .filter((b) => !b.pending && b.parent_bot_id === null && (botUnread[b.id] ?? 0) > 0)
        .map((b) => ({ id: b.id, name: b.name, n: botUnread[b.id] ?? 0 })),
    [bots, botUnread],
  )
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectBot = useStore((s) => s.selectBot)
  if (rows.length === 0) return null
  const turns = rows.reduce((n, r) => n + r.n, 0)
  // 點一下跳到下一顆——停在目前這顆的後面，連點就能一顆一顆掃過去。
  const at = rows.findIndex((r) => r.id === selectedBotId)
  const next = rows[(at + 1) % rows.length]
  const names = rows.map((r) => `${r.name}（${r.n}）`).join('、')
  return (
    <button
      type="button"
      className="unread-chip"
      title={`${rows.length} 顆 Bot 做完了還沒看：${names}。點一下跳到 ${next.name}`}
      aria-label={`${turns} 個回合已完成還沒看，跳到 ${next.name}`}
      onClick={() => selectBot(next.id)}
    >
      <span className="unread-chip-bang" aria-hidden="true">!</span>
      <span className="unread-chip-n">{turns > 99 ? '99+' : turns}</span>
      <span className="unread-chip-name">{next.name}</span>
    </button>
  )
}
