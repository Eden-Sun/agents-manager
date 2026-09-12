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
 * **當前正在看的那一顆要「固定」住**（2026-09-11 使用者，推翻同日稍早「把它排除掉」的做法）：
 * 它不但不消失，還要留在原本的順位上、套 `current` 的 focus 樣式，一眼看出「這顆就是我現在
 * 在看的」。切到別顆之後它才依各組原本的規則離開。
 *
 * 三組的順序都是 `bots` 陣列的順序（不是未讀時間、也不是跑完時間），所以只要一顆晶片還在列
 * 上，它的位置就不會變——不會因為未讀歸零、狀態變了就整列往左跳。
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
  const selectedBotId = useStore((s) => s.selectedBotId)
  // 點進一顆 bot 的那一刻 `selectBot` 會同步把它的未讀清掉（`markBotRead`），所以光看
  // `botUnread` 的話，晶片會在手指底下當場消失、後面幾顆往左跳。使用者要它留在原位。
  // 需要記的只有「它在被選走的前一刻到底有沒有未讀」——位置不用記，這一組的順序本來就是
  // `bots` 的順序。
  const keptId = keepSelectedRow(selectedBotId, botUnread)
  // 只算母 bot（`parent_bot_id === null`）：herdr 開出來的子 agent 是母 bot 自己的工人，
  // 它們回話是給母 bot 看的。team 成員（`b.team`）同樣不算，理由見檔頭。
  const rows = useMemo(
    () =>
      bots
        .filter((b) => tracked(b) && !isPinned.has(b.id) && ((botUnread[b.id] ?? 0) > 0 || b.id === keptId))
        .map((b) => ({ id: b.id, name: b.name, n: botUnread[b.id] ?? 0 })),
    [bots, botUnread, tracked, isPinned, keptId],
  )
  // 「進行中」用 `agent_status` 而不是複合燈號——燈號還混進了主機斷線、啟動中那幾種顏色，
  // 那些不是進行中。
  const runs = useStore((s) => s.runs)
  const working = useMemo(
    () =>
      bots
        .filter((b) => tracked(b) && !isPinned.has(b.id) && (runs[b.id]?.agent_status === 'working' || runs[b.id]?.agent_status === 'blocked'))
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
  const selectedTeamId = useStore((s) => s.selectedTeamId)
  const selectBot = useStore((s) => s.selectBot)
  const selectTeam = useStore((s) => s.selectTeam)
  const botUnreadOf = (id: string) => botUnread[id] ?? 0
  const live = working.length + liveTeams.length
  if (pinned.length === 0 && rows.length === 0 && live === 0) return null
  return (
    <div className="unread-bar" role="status" aria-live="polite">
      {rows.length > 0 ? <span className="unread-bar-label">剛跑完</span> : null}
      {rows.map((r) => (
        <button
          key={r.id}
          type="button"
          className={`unread-chip${r.n > 0 ? ' unread' : ''}${r.id === selectedBotId ? ' current' : ''}`}
          title={r.n > 0 ? `${r.name} 有 ${r.n} 個回合已完成、還沒看過。點一下跳過去` : `${r.name}：你正在看的就是它`}
          aria-current={r.id === selectedBotId ? 'true' : undefined}
          onClick={() => selectBot(r.id)}
        >
          <span className="unread-chip-name">{r.name}</span>
          {/* 黏住的那一顆未讀已經歸零（你就在看它），這時不要畫一顆 `0`。 */}
          {r.n > 0 ? <span className="unread-chip-n">{r.n > 99 ? '99+' : r.n}</span> : null}
        </button>
      ))}
      {/* 釘選的主力：★ 就是標籤，不另外寫字。帶未讀時多套一層 `unread`——見下面的註解。 */}
      {pinned.map((r) => {
        const n = botUnreadOf(r.id)
        return (
          <button
            key={r.id}
            type="button"
            /* `unread` 是**加在釘選身分上的一層狀態**，不是換一組晶片：一排 ★ 看過去，有東西
               等你看的那幾顆要能一眼挑出來，而不是只靠名字後面那個小數字。`current`（你在
               這裡）跟它可以同時成立，兩者的畫法也分得開（見 `unreadChip.css`）。 */
            className={`unread-chip pinned${n > 0 ? ' unread' : ''}${runs[r.id]?.agent_status === 'blocked' ? ' needs-reply' : ''}${r.id === selectedBotId ? ' current' : ''}`}
            title={
              runs[r.id]?.agent_status === 'blocked'
                ? `${r.name}（主要執行的 bot）停在一個要你回答的提示上。點一下過去回答`
                : r.id === selectedBotId
                ? `${r.name}（主要執行的 bot）：你正在看的就是它`
                : n > 0
                  ? `${r.name}（主要執行的 bot）有 ${n} 個回合已完成、還沒看過。點一下跳過去`
                  : `${r.name}（主要執行的 bot）。點一下跳過去`
            }
            aria-current={r.id === selectedBotId ? 'true' : undefined}
            onClick={() => selectBot(r.id)}
          >
            <span className="unread-chip-star" aria-hidden="true">★</span>
            <span className="unread-chip-name">{r.name}</span>
            {n > 0 ? <span className="unread-chip-n">{n > 99 ? '99+' : n}</span> : null}
            {/* 藍點＝還在跑；紅點＝停在要你回答的提示上（`blocked`）。後者跟 `unread` 一樣是疊在
                釘選身分上的一層狀態，不是換一組晶片（2026-09-12 使用者：「星號標注主力處也要」）。 */}
            {runs[r.id]?.agent_status === 'working' || runs[r.id]?.agent_status === 'blocked' ? <span className="unread-chip-dot" aria-hidden="true" /> : null}
          </button>
        )
      })}
      {live > 0 ? (
        <>
          <span className="unread-bar-gap" />
          <span className="unread-bar-label">進行中</span>
          {working.map((w) => (
            <button
              key={w.id}
              type="button"
              className={`unread-chip working${runs[w.id]?.agent_status === 'blocked' ? ' needs-reply' : ''}${w.id === selectedBotId ? ' current' : ''}`}
              title={
                runs[w.id]?.agent_status === 'blocked'
                  ? `${w.name} 停在一個要你回答的提示上。${w.id === selectedBotId ? '你正在看的就是它' : '點一下過去回答'}`
                  : `${w.name} 還在跑。${w.id === selectedBotId ? '你正在看的就是它' : '點一下過去看'}`
              }
              aria-current={w.id === selectedBotId ? 'true' : undefined}
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

/**
 * 「被選走的前一刻，這顆 bot 有沒有未讀？」——有的話回它的 id，讓「剛跑完」那一組把它留著。
 *
 * 為什麼要記：`selectBot` 會同步呼叫 `markBotRead` 把未讀清掉，所以重繪時
 * `botUnread[selectedBotId]` 已經是 0 了，看不出「它剛才在不在這一組」。
 *
 * 為什麼記在模組層而不是 `useState` / `useRef`：`App.tsx` 是 `<ChatPanel key={botId}>`，換 bot
 * 會把整棵（含這一列）重新掛載——元件自己的記憶正好在需要它的那一刻被清空。畫面上同時只有
 * 一列 `.unread-bar`，所以一份模組層的記憶就夠，而且它只從 `selectedBotId` / `botUnread` 推導，
 * 重複呼叫同一組輸入結果一樣（StrictMode 重繪兩次也不會變）。
 *
 * 刻意只記「在不在」，不記位置：這一組是從 `bots` 陣列濾出來的，順序本來就固定。
 * 也刻意不讓「從來沒有未讀過的 bot」黏上來——不然隨便點一顆都會多一顆晶片，三組全空時整列
 * 就不會消失了。
 */
let lastUnread: Record<string, number> = {}
let lastSelected: string | null = null
let kept: string | null = null

function keepSelectedRow(selectedBotId: string | null, botUnread: Record<string, number>): string | null {
  const hasUnread = (id: string | null, table: Record<string, number>) => (id ? (table[id] ?? 0) > 0 : false)
  if (lastSelected !== selectedBotId) {
    // 換人了：用**換之前**那份未讀表看新的這顆原本在不在「剛跑完」裡
    kept = hasUnread(selectedBotId, lastUnread) ? selectedBotId : null
    lastSelected = selectedBotId
  } else if (hasUnread(selectedBotId, botUnread)) {
    // 未讀還沒被清掉（例如視窗在背景時收到的回合）：它本來就在這一組
    kept = selectedBotId
  }
  lastUnread = botUnread
  return kept
}
