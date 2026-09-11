/**
 * 標題列**下面**自成一列，由左到右三組（2026-09-11 使用者定的排法）：
 *
 * - **剛跑完**：跑完了還沒看。要去看的東西排最前面。
 * - 接著是使用者自己用 ★ 釘的「主要執行的 bot」（`PrimaryStar`）。它回答的是「我平常在推的
 *   是哪幾顆」，跟現在有沒有事發生無關，所以**常駐**——只要釘了東西，這一列就在。它**不寫
 *   標籤**：★ 已經說完了（使用者定的），多一個「主力」兩個字只是佔寬度。本來擠在固定 60px
 *   的標題列上（塞不下的收成 `+N`），名字被切成兩三個字反而認不出是誰；移到這一列之後可以
 *   換行，全部都看得見。
 * - **進行中**：還在跑，推到最右邊。
 *
 * 頭尾兩組是會變的狀態；三組都空的時候整列不存在、不佔高度。
 *
 * 釘起來的 bot **不會**同時出現在後兩組：它的晶片本來就帶未讀數與進行中的圓點，再列一次
 * 只是同一件事佔兩格。
 *
 * **當前正在看的那一顆也不列**（2026-09-11 使用者）：它已經是畫面本身，標題列上就寫著同一個
 * 名字，點下去什麼也不會發生。空出來的寬度留給真的要跳過去的那幾顆。
 *
 * 側欄本來就會在每一列上亮未讀（`Sidebar` 的 `.unread-turns`），但側欄在手機上收在抽屜裡、
 * 桌面上也可能被捲掉；使用者要追的是跨 bot 的問題，所以它得待在每個畫面都看得到的地方。
 *
 * 排除規則（2026-09-10 使用者）：
 * - **team 成員不算「剛跑完」**：team 的進度由它的主 issue 管，成員一顆一顆回話不是使用者
 *   要逐則追的東西；真要看去 team 畫面。
 * - **進行中也不列 team 成員**，改成「這個 team 在跑」一顆——同一個 team 三顆成員各佔一格，
 *   說的其實是同一件事。
 * - **AGM（總管）兩邊都不列**：它是管理員，靠例行 loop 醒來，每一輪都會「跑完一回合」。
 *   那不是使用者交代的事，卻會把這兩格洗成永遠有東西。要看它就從側欄的 AGM 總管進去；
 *   真的想常駐追蹤還是可以用 ★ 把它釘起來（釘選是使用者自己指定的，不受這條影響）。
 */
import { useMemo } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Bot } from '../api/types'
import { TEAM_PHASE_LABEL, TEAM_TERMINAL_PHASES } from '../api/types'
import { useStore } from '../store/store'
import './unreadChip.css'

/** 標題列下面那一列：剛跑完、主力（常駐）、進行中。 */
export function UnreadChip() {
  const bots = useStore((s) => s.bots)
  const botUnread = useStore((s) => s.botUnread)
  // AGM 專案（daemon 自己建的總管環境，`supervisor::setup::BOT_NAME`）整個不算——那底下
  // 只有總管與它開出來的工人。
  const agmProjectIds = useStore(useShallow((s) => s.projects.filter((p) => p.label === 'AGM').map((p) => p.id)))
  const tracked = useMemo(
    () => (b: Bot) => !b.pending && b.parent_bot_id === null && !b.team && !agmProjectIds.includes(b.project_id),
    [agmProjectIds],
  )
  // 釘選是使用者自己指定的，所以不受 `tracked` 的排除規則影響（AGM、team 成員、子 agent
  // 都釘得起來）——釘了就是要一直看得到。
  const pinned = useMemo(
    () => bots.filter((b) => !b.pending && b.primary).map((b) => ({ id: b.id, name: b.name })),
    [bots],
  )
  const isPinned = useMemo(() => new Set(pinned.map((p) => p.id)), [pinned])
  // 只算母 bot（`parent_bot_id === null`）：herdr 開出來的子 agent 是母 bot 自己的工人，
  // 它們回話是給母 bot 看的。team 成員（`b.team`）同樣不算，理由見檔頭。
  const rows = useMemo(
    () =>
      bots
        .filter((b) => tracked(b) && !isPinned.has(b.id) && (botUnread[b.id] ?? 0) > 0)
        .map((b) => ({ id: b.id, name: b.name, n: botUnread[b.id] ?? 0 })),
    [bots, botUnread, tracked, isPinned],
  )
  // 「進行中」用 `agent_status` 而不是複合燈號——燈號還混進了主機斷線、啟動中那幾種顏色，
  // 那些不是進行中。
  const runs = useStore((s) => s.runs)
  const working = useMemo(
    () =>
      bots
        .filter((b) => tracked(b) && !isPinned.has(b.id) && runs[b.id]?.agent_status === 'working')
        .map((b) => ({ id: b.id, name: b.name })),
    [bots, runs, tracked, isPinned],
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
  // 排除當前這顆之後才是要畫出來的那一組；`isPinned` 仍用完整的 `pinned`，前後兩組的排除
  // 規則（釘起來的不重複列）不因為「現在在看誰」而改變。
  const pinnedRow = useMemo(() => pinned.filter((p) => p.id !== selectedBotId), [pinned, selectedBotId])
  const selectedTeamId = useStore((s) => s.selectedTeamId)
  const selectBot = useStore((s) => s.selectBot)
  const selectTeam = useStore((s) => s.selectTeam)
  const botUnreadOf = (id: string) => botUnread[id] ?? 0
  const live = working.length + liveTeams.length
  if (pinnedRow.length === 0 && rows.length === 0 && live === 0) return null
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
      {/* 釘選的主力：★ 就是標籤，不另外寫字。 */}
      {pinnedRow.map((r) => (
        <button
          key={r.id}
          type="button"
          className="unread-chip pinned"
          title={`${r.name}（主要執行的 bot）。點一下跳過去`}
          onClick={() => selectBot(r.id)}
        >
          <span className="unread-chip-star" aria-hidden="true">★</span>
          <span className="unread-chip-name">{r.name}</span>
          {botUnreadOf(r.id) > 0 ? (
            <span className="unread-chip-n">{botUnreadOf(r.id) > 99 ? '99+' : botUnreadOf(r.id)}</span>
          ) : null}
          {runs[r.id]?.agent_status === 'working' ? <span className="unread-chip-dot" aria-hidden="true" /> : null}
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
