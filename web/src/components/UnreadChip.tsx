/**
 * 標題列**下面**自成一列：現在有哪幾顆 bot 在等你、在跑、或你釘著要一直看得到。
 *
 * 排法是**緊急度**，不是分組也不是建立時間（2026-09-12 使用者／AGM，推翻 9/11 的三組排法）：
 *
 * 1. `needs-reply`——停在一個要**你本人**回答的提示上。
 * 2. 有未讀——跑完了還沒看。
 * 3. `waits-kids`——自己沒卡住，但在等它開出去的子 agent。
 * 4. `current`——你正在看的那一顆（固定留在列上當定位點）。
 * 5. 其餘（釘選的主力、還在跑的、在跑的 team）。
 *
 * 同一級之內照 `bots` 陣列原本的順序，所以一顆晶片只要還在同一級就不會左右亂跳。
 *
 * 為什麼不再分「剛跑完／進行中」兩塊標籤：分組決定位置的時候，第 9 顆的 `needs-reply` 會
 * 排在最右邊，被捲出視野——使用者 2026-09-12 就這樣錯過了一顆 blocked 的 bot。晶片本來就
 * 各自帶著記號（★、未讀數、紅／黃／藍的點），標籤只是在搶寬度。
 *
 * 版面（同日同一份交辦）：
 * - 桌機不靠橫捲：換行，最多兩行；再多就收起來，右邊那顆 `+N` 按一下展開（不是捲走）。
 * - 手機維持單行橫捲，但要**看得出來能捲**：兩端有陰影與可點的 ◂ ▸，滾輪的垂直滾動在這一列
 *   映射成橫捲，`scroll-snap` 讓邊緣的晶片不會被切一半。
 * - 名字放不下時先縮成 `…`（`.unread-chip-name` 的 ellipsis），不要切掉半顆晶片。
 *
 * 釘起來的 bot 不會重複出現：一顆 bot 就是一顆晶片，該亮的狀態疊在同一顆上。
 *
 * 側欄本來就會在每一列上亮未讀（`Sidebar` 的 `.unread-turns`），但側欄在手機上收在抽屜裡、
 * 桌面上也可能被捲掉；使用者要追的是跨 bot 的問題，所以它得待在每個畫面都看得到的地方。
 *
 * 排除規則（2026-09-10 使用者）：
 * - **team 成員不算**：team 的進度由它的主 issue 管，成員一顆一顆回話不是使用者要逐則追的
 *   東西；整隊只出一顆 `team #N`。
 * - **AGM（總管）不算**：它靠例行 loop 醒來，每一輪都會跑完一回合，會把這一列洗成永遠有東西。
 *   真的想追還是可以用 ★ 釘它（釘選是使用者自己指定的，不受這條影響）。
 */
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type RefObject } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Bot, TeamPhase } from '../api/types'
import { TEAM_PHASE_LABEL, TEAM_TERMINAL_PHASES } from '../api/types'
import { useMediaQuery } from '../hooks/useMediaQuery'
import { useStore } from '../store/store'
import './unreadChip.css'

/** 跟 `unreadChip.css` 裡那個斷點同一個值：以下是手機的單行橫捲，以上是桌機的兩行換行。 */
const NARROW_QUERY = '(max-width: 720px)'

/** 緊急度。數字小的排前面（見檔頭）。 */
const RANK = { needsReply: 0, unread: 1, waitsKids: 2, current: 3, rest: 4 } as const

interface ChipItem {
  id: string
  name: string
  /** 點下去要開的東西：bot 的對話，或 team 畫面。 */
  go: () => void
  rank: number
  current: boolean
  pinned: boolean
  unread: number
  needsReply: boolean
  waitsKids: boolean
  working: boolean
  title: string
}

/** 標題列下面那一列。 */
export function UnreadChip() {
  const bots = useStore((s) => s.bots)
  const botUnread = useStore((s) => s.botUnread)
  const runs = useStore((s) => s.runs)
  const teams = useStore((s) => s.teams)
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectedTeamId = useStore((s) => s.selectedTeamId)
  const selectBot = useStore((s) => s.selectBot)
  const selectTeam = useStore((s) => s.selectTeam)
  // AGM 專案（daemon 自己建的總管環境）整個不算——那底下只有總管與它開出去的工人。
  const agmProjectIds = useStore(useShallow((s) => s.projects.filter((p) => p.label === 'AGM').map((p) => p.id)))
  // 點進一顆 bot 的那一刻 `selectBot` 會同步清掉它的未讀，所以光看 `botUnread`，晶片會在手指
  // 底下當場消失。記住「它在被選走的前一刻有沒有未讀」，讓它留在列上。
  const keptId = keepSelectedRow(selectedBotId, botUnread)

  const items = useMemo(() => {
    const tracked = (b: Bot) => !b.pending && b.parent_bot_id === null && !b.team && !agmProjectIds.includes(b.project_id)
    const waitsKids = (id: string) =>
      bots.some((b) => b.parent_bot_id === id && (runs[b.id]?.agent_status === 'working' || runs[b.id]?.agent_status === 'blocked'))
    const out: ChipItem[] = []
    for (const b of bots) {
      if (b.pending) continue
      // 釘選是使用者自己指定的，所以不受排除規則影響（AGM、team 成員、子 agent 都釘得起來）。
      const pinned = b.primary
      const n = botUnread[b.id] ?? 0
      const status = runs[b.id]?.agent_status
      const needsReply = status === 'blocked'
      const kids = !needsReply && waitsKids(b.id)
      const current = b.id === selectedBotId && !selectedTeamId
      const show = pinned || (tracked(b) && (n > 0 || b.id === keptId || status === 'working' || needsReply))
      if (!show) continue
      out.push({
        id: b.id,
        name: b.name,
        go: () => selectBot(b.id),
        rank: needsReply ? RANK.needsReply : n > 0 ? RANK.unread : kids ? RANK.waitsKids : current ? RANK.current : RANK.rest,
        current,
        pinned,
        unread: n,
        needsReply,
        waitsKids: kids,
        working: status === 'working',
        title: botTitle(b.name, pinned, n, needsReply, kids, current),
      })
    }
    // team 用整隊一顆：還在跑的 team（phase 未進終態、也不是暫停）。
    const live = Object.values(teams)
      .filter((t) => !TEAM_TERMINAL_PHASES.includes(t.phase) && t.phase !== 'paused')
      .sort((a, b) => a.created_at.localeCompare(b.created_at))
    for (const t of live) {
      const current = t.id === selectedTeamId
      out.push({
        id: `team:${t.id}`,
        name: `team #${t.issue_number}`,
        go: () => selectTeam(t.id),
        rank: current ? RANK.current : RANK.rest,
        current,
        pinned: false,
        unread: 0,
        needsReply: false,
        waitsKids: false,
        working: true,
        title: teamTitle(t.issue_number, t.issue_title, t.phase, current),
      })
    }
    // 穩定排序：同一級之內維持上面推進去的順序（＝`bots` 的順序，team 在最後）。
    return out.map((it, i) => ({ it, i })).sort((a, b) => a.it.rank - b.it.rank || a.i - b.i).map((x) => x.it)
  }, [agmProjectIds, botUnread, bots, keptId, runs, selectBot, selectTeam, selectedBotId, selectedTeamId, teams])

  const barRef = useRef<HTMLDivElement | null>(null)
  const narrow = useMediaQuery(NARROW_QUERY)
  const [expanded, setExpanded] = useState(false)
  const hidden = useOverflowRows(barRef, !narrow && !expanded, items.length)
  const scroll = useHorizontalScroll(barRef, narrow)
  // 換 bot（或這一列的組成變了）就把 `current` 那顆捲進畫面。`aria-current` 當選擇器。
  useScrollCurrentIntoView(barRef, `${selectedBotId}/${selectedTeamId}/${items.length}`)

  if (items.length === 0) return null
  const clipped = !narrow && !expanded && hidden > 0
  return (
    <div className={`unread-bar-wrap${scroll.left ? ' can-left' : ''}${scroll.right ? ' can-right' : ''}`}>
      {/* 能捲才畫箭頭：只有陰影的話使用者看不出這裡還有東西（2026-09-12 使用者回報）。 */}
      {scroll.left ? (
        <button type="button" className="unread-bar-arrow left" aria-label="往左看更多" onClick={() => scroll.by(-1)}>
          ◂
        </button>
      ) : null}
      <div
        className={`unread-bar${clipped ? ' clipped' : ''}${expanded ? ' expanded' : ''}`}
        ref={barRef}
        role="status"
        aria-live="polite"
      >
        {items.map((it) => (
          <button
            key={it.id}
            type="button"
            className={chipClass(it)}
            title={it.title}
            aria-current={it.current ? 'true' : undefined}
            onClick={it.go}
          >
            {it.pinned ? (
              <span className="unread-chip-star" aria-hidden="true">
                ★
              </span>
            ) : null}
            <span className="unread-chip-name">{it.name}</span>
            {it.unread > 0 ? <span className="unread-chip-n">{it.unread > 99 ? '99+' : it.unread}</span> : null}
            {/* 藍＝還在跑、紅＝等你回答、黃＝等子 agent。 */}
            {it.working || it.needsReply || it.waitsKids ? <span className="unread-chip-dot" aria-hidden="true" /> : null}
          </button>
        ))}
      </div>
      {scroll.right ? (
        <button type="button" className="unread-bar-arrow right" aria-label="往右看更多" onClick={() => scroll.by(1)}>
          ▸
        </button>
      ) : null}
      {/* 桌機超過兩行：收起來的那幾顆一定是最不急的（排序見檔頭），按一下展開。 */}
      {!narrow && (hidden > 0 || expanded) ? (
        <button
          type="button"
          className="unread-bar-more"
          aria-expanded={expanded}
          title={expanded ? '收合成兩行' : `還有 ${hidden} 顆沒顯示（都是比較不急的）。點一下展開`}
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded ? '收合' : `+${hidden}`}
        </button>
      ) : null}
    </div>
  )
}

function chipClass(it: ChipItem): string {
  const state = it.needsReply ? ' needs-reply' : it.waitsKids ? ' waits-kids' : it.unread > 0 ? ' unread' : it.working ? ' working' : ''
  return `unread-chip${it.pinned ? ' pinned' : ''}${state}${it.current ? ' current' : ''}`
}

function botTitle(name: string, pinned: boolean, n: number, needsReply: boolean, kids: boolean, current: boolean): string {
  const who = pinned ? `${name}（主要執行的 bot）` : name
  const tail = current ? '你正在看的就是它' : '點一下跳過去'
  if (needsReply) return `${who} 停在一個要你回答的提示上。${current ? tail : '點一下過去回答'}`
  if (n > 0) return `${who} 有 ${n} 個回合已完成、還沒看過。${tail}`
  if (kids) return `${who} 在等子 agent 完成。${tail}`
  return `${who}。${tail}`
}

function teamTitle(issue: number, title: string, phase: TeamPhase, current: boolean): string {
  return `Team #${issue}・${title}・${TEAM_PHASE_LABEL[phase]}。${current ? '你正在看的就是它' : '點一下開 team 畫面'}`
}

/**
 * 兩行裝不下的有幾顆。
 *
 * 用量到的是每顆晶片的 `offsetTop`：同一行的值一樣，所以不同的 `offsetTop` 就是行。第三行
 * 起的全部算「沒顯示」——CSS 那邊 `.clipped` 只留兩行的高度，所以它們本來就看不到了。
 *
 * 刻意不去算「還能塞幾顆」再切陣列：那會讓 render 依賴自己的測量結果，一改就震盪。這裡
 * 永遠把全部畫出來、只是把超出的裁掉，測量只影響右邊那顆 `+N` 的數字。
 */
function useOverflowRows(barRef: RefObject<HTMLDivElement | null>, active: boolean, count: number): number {
  const [hidden, setHidden] = useState(0)
  const measure = useCallback(() => {
    const bar = barRef.current
    if (!bar || !active) {
      setHidden((prev) => (prev === 0 ? prev : 0))
      return
    }
    const chips = [...bar.querySelectorAll<HTMLElement>('.unread-chip')]
    const rows: number[] = []
    for (const c of chips) if (!rows.includes(c.offsetTop)) rows.push(c.offsetTop)
    const cut = rows[1]
    // 只在數字真的變了才寫 state：測量是在 layout effect 裡跑的（render 期間量不到
    // `offsetTop`），同一個值重複寫會讓它每一幀都重繪一次。
    const n = rows.length <= 2 ? 0 : chips.filter((c) => c.offsetTop > cut).length
    setHidden((prev) => (prev === n ? prev : n))
  }, [active, barRef])
  useLayoutEffect(measure, [measure, count])
  useEffect(() => {
    const bar = barRef.current
    if (!bar || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(measure)
    ro.observe(bar)
    return () => ro.disconnect()
  }, [barRef, measure])
  return hidden
}

/**
 * 手機那條單行橫捲：滾輪（垂直）映射成橫捲，兩端能不能再捲用 state 記著，讓 ◂ ▸ 與陰影
 * 只有在真的捲得動時才出現。
 *
 * `preventDefault` 只在**真的捲得動**時做，否則在這一列上滾滑鼠會把整頁鎖住。
 */
function useHorizontalScroll(barRef: RefObject<HTMLDivElement | null>, active: boolean) {
  const [edges, setEdges] = useState({ left: false, right: false })
  const sync = useCallback(() => {
    const bar = barRef.current
    if (!bar || !active) {
      setEdges((e) => (e.left || e.right ? { left: false, right: false } : e))
      return
    }
    const max = bar.scrollWidth - bar.clientWidth
    const next = { left: bar.scrollLeft > 4, right: max > 4 && bar.scrollLeft < max - 4 }
    setEdges((e) => (e.left === next.left && e.right === next.right ? e : next))
  }, [active, barRef])
  useEffect(() => {
    const bar = barRef.current
    if (!bar) return
    sync()
    // 滾輪要原生、非 passive 地掛：React 的 onWheel 是 passive，裡面的 preventDefault 無效，
    // 「到底才把滾動還給整頁」的分流就失效，列橫捲的同時整頁也直捲。
    const onWheel = (e: WheelEvent) => {
      if (!active) return
      const max = bar.scrollWidth - bar.clientWidth
      if (max <= 4 || Math.abs(e.deltaY) <= Math.abs(e.deltaX)) return
      const next = Math.min(max, Math.max(0, bar.scrollLeft + e.deltaY))
      if (next === bar.scrollLeft) return // 已經到底：把滾動還給整頁
      e.preventDefault()
      bar.scrollLeft = next
    }
    bar.addEventListener('scroll', sync, { passive: true })
    bar.addEventListener('wheel', onWheel, { passive: false })
    const ro = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(sync)
    ro?.observe(bar)
    return () => {
      bar.removeEventListener('scroll', sync)
      bar.removeEventListener('wheel', onWheel)
      ro?.disconnect()
    }
  }, [barRef, sync, active])
  const by = (dir: 1 | -1) => {
    const bar = barRef.current
    if (!bar) return
    bar.scrollBy({ left: dir * Math.max(120, bar.clientWidth * 0.6), behavior: 'smooth' })
  }
  return { ...edges, by }
}

/**
 * 把 `current` 那顆捲進畫面（只在這一列橫捲時有事做）。
 *
 * 不用 `Element.scrollIntoView`：它會連帶捲祖先，在手機上會把整個 `.app` 往旁邊推一格；
 * 這裡只動這一列自己的 `scrollLeft`。已經看得見就完全不動——不然每次重繪都把列拉回中間，
 * 使用者自己捲到的位置會被搶走。
 */
function useScrollCurrentIntoView(barRef: RefObject<HTMLDivElement | null>, key: string) {
  useLayoutEffect(() => {
    const bar = barRef.current
    if (!bar) return
    const chip = bar.querySelector<HTMLElement>('.unread-chip[aria-current="true"]')
    if (!chip) return
    const pad = 16
    const left = chip.offsetLeft - bar.scrollLeft
    if (left >= pad && left + chip.offsetWidth <= bar.clientWidth - pad) return
    bar.scrollLeft = Math.max(0, chip.offsetLeft - (bar.clientWidth - chip.offsetWidth) / 2)
  }, [barRef, key])
}

/**
 * 「被選走的前一刻，這顆 bot 有沒有未讀？」——有的話回它的 id，讓它留在列上。
 *
 * 為什麼要記：`selectBot` 會同步呼叫 `markBotRead` 把未讀清掉，所以重繪時
 * `botUnread[selectedBotId]` 已經是 0 了，看不出「它剛才在不在這一列」。
 *
 * 為什麼記在模組層而不是 `useState` / `useRef`：`App.tsx` 是 `<ChatPanel key={botId}>`，換 bot
 * 會把整棵（含這一列）重新掛載——元件自己的記憶正好在需要它的那一刻被清空。畫面上同時只有
 * 一列 `.unread-bar`，所以一份模組層的記憶就夠，而且它只從 `selectedBotId` / `botUnread` 推導。
 */
let lastUnread: Record<string, number> = {}
let lastSelected: string | null = null
let kept: string | null = null

function keepSelectedRow(selectedBotId: string | null, botUnread: Record<string, number>): string | null {
  const hasUnread = (id: string | null, table: Record<string, number>) => (id ? (table[id] ?? 0) > 0 : false)
  if (lastSelected !== selectedBotId) {
    kept = hasUnread(selectedBotId, lastUnread) ? selectedBotId : null
    lastSelected = selectedBotId
  } else if (hasUnread(selectedBotId, botUnread)) {
    kept = selectedBotId
  }
  lastUnread = botUnread
  return kept
}
