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
  // 右半邊：現在**還在跑**的母 bot。左邊是「回來了、去看」，右邊是「還在做、別等它」——
  // 兩件事在同一列上，一眼就知道自己丟出去的工作各自到哪了。用 `agent_status` 而不是燈號：
  // 燈號還混進了主機斷線、啟動中那幾種顏色，那些不是「進行中」。
  const runs = useStore((s) => s.runs)
  const working = useMemo(
    () =>
      bots
        .filter((b) => !b.pending && b.parent_bot_id === null && runs[b.id]?.agent_status === 'working')
        .map((b) => ({ id: b.id, name: b.name })),
    [bots, runs],
  )
  // 使用者自己釘的「主要執行的 bot」（`PrimaryStar`，存在 daemon）。這一排跟未讀無關：
  // 不管有沒有回覆、在不在跑都固定排在最上面，因為它回答的是另一個問題——「我平常在推的是
  // 哪幾顆」。順序照側欄（`bots` 本來就排好了）。
  const primary = useMemo(() => bots.filter((b) => !b.pending && b.primary).map((b) => ({ id: b.id, name: b.name })), [bots])
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectBot = useStore((s) => s.selectBot)
  if (rows.length === 0 && working.length === 0 && primary.length === 0) return null
  // 標題列（固定 60px、擠滿了燈號／額度／分頁）放不下，所以自成一列掛在它下面：
  // 一顆 bot 一個晶片，點下去就跳過去看——不用先猜「下一個」是誰。
  return (
    <>
      {primary.length > 0 ? (
        <div className="unread-bar primary-bar">
          <span className="unread-bar-label">主要</span>
          {primary.map((r) => (
            <button
              key={r.id}
              type="button"
              className={`unread-chip pinned${r.id === selectedBotId ? ' current' : ''}`}
              title={`${r.name}（主要執行的 bot）。點一下跳過去`}
              onClick={() => selectBot(r.id)}
            >
              <span className="unread-chip-star" aria-hidden="true">★</span>
              <span className="unread-chip-name">{r.name}</span>
              {(botUnread[r.id] ?? 0) > 0 ? <span className="unread-chip-n">{botUnread[r.id]}</span> : null}
              {runs[r.id]?.agent_status === 'working' ? <span className="unread-chip-dot" aria-hidden="true" /> : null}
            </button>
          ))}
        </div>
      ) : null}
      {rows.length > 0 || working.length > 0 ? (
        <div className="unread-bar" role="status" aria-live="polite">
          {rows.length > 0 ? <span className="unread-bar-label">剛跑完</span> : null}
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
          {working.length > 0 ? (
            <>
              <span className="unread-bar-gap" />
              <span className="unread-bar-label">進行中</span>
              {working.map((w) => (
                <button
                  key={w.id}
                  type="button"
                  className={`unread-chip working${w.id === selectedBotId ? ' current' : ''}`}
                  title={`${w.name} 還在跑。點一下過去看`}
                  onClick={() => selectBot(w.id)}
                >
                  <span className="unread-chip-dot" aria-hidden="true" />
                  <span className="unread-chip-name">{w.name}</span>
                </button>
              ))}
            </>
          ) : null}
        </div>
      ) : null}
    </>
  )
}
