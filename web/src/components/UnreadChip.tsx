/**
 * 標題列**下面那一列**：「哪幾顆母 bot 做完了、還沒看」。
 *
 * 側欄本來就會在每一列上亮未讀（`Sidebar` 的 `.unread-turns`），但側欄在手機上是收起來的、
 * 在桌面上也可能被捲到看不見的位置。使用者真正想追的是「我丟出去的那幾件事，哪些回來了」，
 * 那是一個跨 bot 的問題，所以它要待在每個畫面都看得到的地方。
 *
 * 為什麼不塞進標題列本身：`.main-head` 是固定 60px 的單行 flex，燈號、模型、額度、分頁
 * 已經在搶那條線；再擠一顆進去只會把別人擠掉。自成一列反而放得下每一顆 bot 的名字。
 * 一顆都沒有未讀時整列不佔高度（回傳 null）。
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
  // 標題列（固定 60px、擠滿了燈號／額度／分頁）放不下，所以自成一列掛在它下面：
  // 一顆 bot 一個晶片，點下去就跳過去看——不用先猜「下一個」是誰。
  return (
    <div className="unread-bar" role="status" aria-live="polite">
      <span className="unread-bar-label">剛跑完</span>
      {rows.map((r) => (
        <button
          key={r.id}
          type="button"
          className={`unread-chip${r.id === selectedBotId ? ' current' : ''}`}
          title={`${r.name} 有 ${r.n} 個回合已完成、還沒看過。點一下跳過去`}
          onClick={() => selectBot(r.id)}
        >
          <span className="unread-chip-name">{r.name}</span>
          <span className="unread-chip-n">{r.n > 99 ? '99+' : r.n}</span>
        </button>
      ))}
    </div>
  )
}
