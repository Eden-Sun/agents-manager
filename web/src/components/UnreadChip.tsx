/**
 * 標題列右上角（分頁鍵左邊）的那一小排：「哪幾顆做完了、什麼還在跑、我釘了哪幾顆」。
 *
 * 側欄本來就會在每一列上亮未讀（`Sidebar` 的 `.unread-turns`），但側欄在手機上是收起來的、
 * 在桌面上也可能被捲到看不見的位置。使用者真正想追的是「我丟出去的那幾件事，哪些回來了」，
 * 那是一個跨 bot 的問題，所以它要待在每個畫面都看得到的地方。
 *
 * 排除規則（2026-09-10 使用者）：
 * - **team 成員不算「剛跑完」**：team 的進度由它的主 issue 管，成員一顆一顆回話不是使用者
 *   要逐則追的東西；真要看去 team 畫面。
 * - **進行中也不列 team 成員**，改成「這個 team 在跑」一顆——同一個 team 三顆成員各佔一格，
 *   說的其實是同一件事。
 *
 * 位置在 `.main-head` 裡、分頁鍵（對話／終端）左邊，所以每一顆都要能在一行裡收好：
 * 名字會截斷，整排不換行，什麼都沒有時回傳 null（標題列一格都不佔）。
 */
import { useMemo } from 'react'
import { TEAM_PHASE_LABEL, TEAM_TERMINAL_PHASES } from '../api/types'
import { useStore } from '../store/store'
import './unreadChip.css'

export function UnreadChip() {
  // 兩個欄位分開選、在 `useMemo` 裡才組成陣列：selector 每次回一個新陣列會讓 zustand
  // 每一幀都判定「變了」，畫面就停不下來。
  const bots = useStore((s) => s.bots)
  const botUnread = useStore((s) => s.botUnread)
  // 只算母 bot（`parent_bot_id === null`）：herdr 開出來的子 agent 是母 bot 自己的工人，
  // 它們回話是給母 bot 看的，算進來只會讓數字比使用者實際要處理的事情多。
  // team 成員（`b.team`）同樣不算：team 的進度看它的主 issue，不是看成員各自回了幾句。
  const rows = useMemo(
    () =>
      bots
        .filter((b) => !b.pending && b.parent_bot_id === null && !b.team && (botUnread[b.id] ?? 0) > 0)
        .map((b) => ({ id: b.id, name: b.name, n: botUnread[b.id] ?? 0 })),
    [bots, botUnread],
  )
  // 「進行中」：還在跑的母 bot。用 `agent_status` 而不是複合燈號——燈號還混進了主機斷線、
  // 啟動中那幾種顏色，那些不是進行中。team 成員一樣不列。
  const runs = useStore((s) => s.runs)
  const working = useMemo(
    () =>
      bots
        .filter((b) => !b.pending && b.parent_bot_id === null && !b.team && runs[b.id]?.agent_status === 'working')
        .map((b) => ({ id: b.id, name: b.name })),
    [bots, runs],
  )
  // team 用整隊一顆：進行中的 team（phase 還沒進終態、也不是暫停）點下去開 team 畫面。
  const teams = useStore((s) => s.teams)
  const liveTeams = useMemo(
    () =>
      Object.values(teams)
        .filter((t) => !TEAM_TERMINAL_PHASES.includes(t.phase) && t.phase !== 'paused')
        .sort((a, b) => a.created_at.localeCompare(b.created_at))
        .map((t) => ({ id: t.id, label: `#${t.issue_number}`, title: t.issue_title, phase: t.phase })),
    [teams],
  )
  // 使用者自己釘的「主要執行的 bot」（`PrimaryStar`，存在 daemon）。這一排跟未讀無關：
  // 不管有沒有回覆、在不在跑都固定排在最上面，因為它回答的是另一個問題——「我平常在推的是
  // 哪幾顆」。順序照側欄（`bots` 本來就排好了）。
  const primary = useMemo(() => bots.filter((b) => !b.pending && b.primary).map((b) => ({ id: b.id, name: b.name })), [bots])
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectedTeamId = useStore((s) => s.selectedTeamId)
  const selectBot = useStore((s) => s.selectBot)
  const selectTeam = useStore((s) => s.selectTeam)
  if (rows.length === 0 && working.length === 0 && primary.length === 0 && liveTeams.length === 0) return null
  // 標題列（固定 60px、擠滿了燈號／額度／分頁）放不下，所以自成一列掛在它下面：
  // 一顆 bot 一個晶片，點下去就跳過去看——不用先猜「下一個」是誰。
  // 標題列只有那麼寬，塞不下的就收成一顆 `+N`（點它跳到第一個被藏起來的）。順序＝重要性：
  // 釘選的 → 剛跑完 → 還在跑 → 在跑的 team。
  const MAX = 3
  const all = [
    ...primary.map((r) => ({ kind: 'pinned' as const, id: r.id, name: r.name })),
    ...rows.map((r) => ({ kind: 'done' as const, id: r.id, name: r.name, n: r.n })),
    ...working.map((w) => ({ kind: 'working' as const, id: w.id, name: w.name })),
    ...liveTeams.map((t) => ({ kind: 'team' as const, id: t.id, name: `team ${t.label}`, title: t.title, phase: t.phase })),
  ]
  const shown = all.slice(0, MAX)
  const hidden = all.slice(MAX)
  const go = (item: (typeof all)[number]) => (item.kind === 'team' ? selectTeam(item.id) : selectBot(item.id))
  return (
    <div className="unread-strip" role="status" aria-live="polite">
      {shown.map((it) =>
        it.kind === 'pinned' ? (
          <button
            key={it.id}
            type="button"
            className={`unread-chip pinned${it.id === selectedBotId ? ' current' : ''}`}
            title={`${it.name}（主要執行的 bot）。點一下跳過去`}
            onClick={() => go(it)}
          >
            <span className="unread-chip-star" aria-hidden="true">★</span>
            <span className="unread-chip-name">{it.name}</span>
            {(botUnread[it.id] ?? 0) > 0 ? <span className="unread-chip-n">{botUnread[it.id]}</span> : null}
            {runs[it.id]?.agent_status === 'working' ? <span className="unread-chip-dot" aria-hidden="true" /> : null}
          </button>
        ) : it.kind === 'done' ? (
          <button
            key={it.id}
            type="button"
            className={`unread-chip${it.id === selectedBotId ? ' current' : ''}`}
            title={`${it.name} 有 ${it.n} 個回合已完成、還沒看過。點一下跳過去`}
            onClick={() => go(it)}
          >
            <span className="unread-chip-name">{it.name}</span>
            <span className="unread-chip-n">{it.n > 99 ? '99+' : it.n}</span>
          </button>
        ) : (
          <button
            key={it.id}
            type="button"
            className={`unread-chip working${it.kind === 'team' ? ' team' : ''}${
              it.id === (it.kind === 'team' ? selectedTeamId : selectedBotId) ? ' current' : ''
            }`}
            title={
              it.kind === 'team'
                ? `Team ${it.name}・${it.title}・${TEAM_PHASE_LABEL[it.phase]}。點一下開 team 畫面`
                : `${it.name} 還在跑。點一下過去看`
            }
            onClick={() => go(it)}
          >
            <span className="unread-chip-dot" aria-hidden="true" />
            <span className="unread-chip-name">{it.name}</span>
          </button>
        ),
      )}
      {hidden.length > 0 ? (
        <button
          type="button"
          className="unread-chip more"
          title={`還有 ${hidden.map((h) => h.name).join('、')}。點一下跳到 ${hidden[0].name}`}
          onClick={() => go(hidden[0])}
        >
          +{hidden.length}
        </button>
      ) : null}
    </div>
  )
}
