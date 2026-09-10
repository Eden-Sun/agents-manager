/**
 * 兩個位置、兩件事（2026-09-10 使用者定的分工）：
 *
 * - `PrimaryChips`：標題列右上角、分頁鍵（對話／終端）左邊。使用者自己用 ★ 釘的
 *   「主要執行的 bot」（`PrimaryStar`）。它回答的是「我平常在推的是哪幾顆」，跟現在有沒有
 *   事發生無關，所以常駐在標題列上；標題列是固定 60px 的單行，塞不下的收成一顆 `+N`。
 * - `UnreadChip`：標題列**下面**自成一列。左邊「剛跑完」（跑完了還沒看），右邊「進行中」
 *   （還在跑）。這兩件事是會變的狀態，兩邊都空的時候整列不存在、不佔高度。
 *
 * 側欄本來就會在每一列上亮未讀（`Sidebar` 的 `.unread-turns`），但側欄在手機上收在抽屜裡、
 * 桌面上也可能被捲掉；使用者要追的是跨 bot 的問題，所以它得待在每個畫面都看得到的地方。
 *
 * 排除規則（2026-09-10 使用者）：
 * - **team 成員不算「剛跑完」**：team 的進度由它的主 issue 管，成員一顆一顆回話不是使用者
 *   要逐則追的東西；真要看去 team 畫面。
 * - **進行中也不列 team 成員**，改成「這個 team 在跑」一顆——同一個 team 三顆成員各佔一格，
 *   說的其實是同一件事。
 */
import { useMemo } from 'react'
import { TEAM_PHASE_LABEL, TEAM_TERMINAL_PHASES } from '../api/types'
import { useStore } from '../store/store'
import './unreadChip.css'

/** 標題列右上角那一小排：使用者釘選的主要 bot。塞不下的收成 `+N`。 */
export function PrimaryChips() {
  // 欄位分開選、在 `useMemo` 裡才組成陣列：selector 每次回一個新陣列會讓 zustand 每一幀
  // 都判定「變了」，畫面就停不下來。
  const bots = useStore((s) => s.bots)
  const botUnread = useStore((s) => s.botUnread)
  const runs = useStore((s) => s.runs)
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectBot = useStore((s) => s.selectBot)
  const primary = useMemo(() => bots.filter((b) => !b.pending && b.primary).map((b) => ({ id: b.id, name: b.name })), [bots])
  if (primary.length === 0) return null
  // 標題列只有那麼寬：超過的收成一顆 `+N`，點它跳到第一個被藏起來的。
  const MAX = 3
  const shown = primary.slice(0, MAX)
  const hidden = primary.slice(MAX)
  return (
    <div className="unread-strip">
      {shown.map((r) => (
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
      {hidden.length > 0 ? (
        <button
          type="button"
          className="unread-chip more"
          title={`還有 ${hidden.map((h) => h.name).join('、')}。點一下跳到 ${hidden[0].name}`}
          onClick={() => selectBot(hidden[0].id)}
        >
          +{hidden.length}
        </button>
      ) : null}
    </div>
  )
}

/** 標題列下面那一列：左邊「剛跑完」、右邊「進行中」。 */
export function UnreadChip() {
  const bots = useStore((s) => s.bots)
  const botUnread = useStore((s) => s.botUnread)
  // 只算母 bot（`parent_bot_id === null`）：herdr 開出來的子 agent 是母 bot 自己的工人，
  // 它們回話是給母 bot 看的。team 成員（`b.team`）同樣不算，理由見檔頭。
  const rows = useMemo(
    () =>
      bots
        .filter((b) => !b.pending && b.parent_bot_id === null && !b.team && (botUnread[b.id] ?? 0) > 0)
        .map((b) => ({ id: b.id, name: b.name, n: botUnread[b.id] ?? 0 })),
    [bots, botUnread],
  )
  // 「進行中」用 `agent_status` 而不是複合燈號——燈號還混進了主機斷線、啟動中那幾種顏色，
  // 那些不是進行中。
  const runs = useStore((s) => s.runs)
  const working = useMemo(
    () =>
      bots
        .filter((b) => !b.pending && b.parent_bot_id === null && !b.team && runs[b.id]?.agent_status === 'working')
        .map((b) => ({ id: b.id, name: b.name })),
    [bots, runs],
  )
  // team 用整隊一顆：還在跑的 team（phase 未進終態、也不是暫停），點下去開 team 畫面。
  const teams = useStore((s) => s.teams)
  const liveTeams = useMemo(
    () =>
      Object.values(teams)
        .filter((t) => !TEAM_TERMINAL_PHASES.includes(t.phase) && t.phase !== 'paused')
        .sort((a, b) => a.created_at.localeCompare(b.created_at))
        .map((t) => ({ id: t.id, label: `#${t.issue_number}`, title: t.issue_title, phase: t.phase })),
    [teams],
  )
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectedTeamId = useStore((s) => s.selectedTeamId)
  const selectBot = useStore((s) => s.selectBot)
  const selectTeam = useStore((s) => s.selectTeam)
  const live = working.length + liveTeams.length
  if (rows.length === 0 && live === 0) return null
  return (
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
      {live > 0 ? (
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
          {liveTeams.map((t) => (
            <button
              key={t.id}
              type="button"
              className={`unread-chip working team${t.id === selectedTeamId ? ' current' : ''}`}
              title={`Team ${t.label}・${t.title}・${TEAM_PHASE_LABEL[t.phase]}。點一下開 team 畫面`}
              onClick={() => selectTeam(t.id)}
            >
              <span className="unread-chip-dot" aria-hidden="true" />
              <span className="unread-chip-name">team {t.label}</span>
            </button>
          ))}
        </>
      ) : null}
    </div>
  )
}
