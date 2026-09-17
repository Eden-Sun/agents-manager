/**
 * 標題列下面一列：等你、在跑、或釘選的 bot；一顆 bot 一顆晶片，狀態疊在同一顆上。
 *
 * 排序＝緊急度（2026-09-12 使用者／AGM，推翻 9/11 分組）：needs-reply → 未讀 → waits-kids → current → 其餘；
 * 同級照 `bots` 順序。不分組標籤：分組會把第 9 顆 needs-reply 擠出視野（使用者 2026-09-12 錯過 blocked bot）。
 * 手機不排序（2026-09-13 使用者：「手機版星號列不要任意改變順序」）：單行橫捲靠位置肌肉記憶。
 * 版面（同日）：桌機換行最多兩行、`+N` 展開；手機單行橫捲要看得出能捲（陰影＋◂ ▸、滾輪映射、scroll-snap）。
 * 兩排（2026-09-15 使用者：「非標主力之現執行中與剛完成的 bot 要出現在主力的下一排」）：★ 主力一排，
 * 沒釘的（在跑、剛跑完、要回答）一定換到下一排；各排裡照上面的排序。手機單行橫捲放不下兩排，照同樣分組
 * 分成上下兩排，各自橫捲（2026-09-16 使用者：「已完成放下一排，方便我點選」）。
 * 排除（2026-09-10 使用者）：AGM 總管專案不算（例行 loop 會洗版），但 ★ 釘選不受影響；
 * 認 `GET /api/supervisor` 的 `project_id` 不認名字（2026-09-13 已從 `AGM` 改名 `AGM-DM-GRUP`）。
 */
import { Fragment, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type RefObject } from 'react'
import type { Bot } from '../api/types'
import { useMediaQuery } from '../hooks/useMediaQuery'
import { chipTracked } from '../lib/supervisorProject'
import { useStore } from '../store/store'
import './unreadChip.css'

/** 與 `unreadChip.css` 斷點同值。 */
const NARROW_QUERY = '(max-width: 720px)'

const RANK = { needsReply: 0, unread: 1, waitsKids: 2, current: 3, rest: 4 } as const

interface ChipItem {
  id: string
  name: string
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

function Chip({ it }: { it: ChipItem }) {
  return (
    <button type="button" className={chipClass(it)} title={it.title} aria-current={it.current ? 'true' : undefined} onClick={it.go}>
      {it.pinned ? (
        <span className="unread-chip-star" aria-hidden="true">
          ★
        </span>
      ) : null}
      <span className="unread-chip-name">{it.name}</span>
      {it.unread > 0 ? <span className="unread-chip-n">{it.unread > 99 ? '99+' : it.unread}</span> : null}
      {it.working || it.needsReply || it.waitsKids ? <span className="unread-chip-dot" aria-hidden="true" /> : null}
    </button>
  )
}

/**
 * 手機的一排：自己橫捲、自己一組箭頭。★ 主力一排、在跑／剛完成的另一排（2026-09-16 使用者：
 * 「已完成放下一排，方便我點選」），不必先把主力捲過去才點得到剛跑完的那顆。
 */
function ScrollRow({ items, label, selectedBotId }: { items: ChipItem[]; label: string; selectedBotId: string | null }) {
  // 空的那排整個不掛：捲動 hook 的 effect 只在掛載時讀 `barRef`，空著掛上去讀到 null 就再也不接
  // scroll／wheel／ResizeObserver——之後晶片出現、溢出了也沒有 ◂ ▸（review3 c5 L1）。
  if (items.length === 0) return null
  return <ScrollRowBar items={items} label={label} selectedBotId={selectedBotId} />
}

function ScrollRowBar({ items, label, selectedBotId }: { items: ChipItem[]; label: string; selectedBotId: string | null }) {
  const barRef = useRef<HTMLDivElement | null>(null)
  const scroll = useHorizontalScroll(barRef, true)
  useScrollCurrentIntoView(barRef, `${selectedBotId}/${items.length}`)
  return (
    <div className={`unread-bar-wrap row${scroll.left ? ' can-left' : ''}${scroll.right ? ' can-right' : ''}`}>
      {scroll.left ? (
        <button type="button" className="unread-bar-arrow left" aria-label="往左看更多" onClick={() => scroll.by(-1)}>
          ◂
        </button>
      ) : null}
      <div className="unread-bar" ref={barRef} role="status" aria-live="polite" aria-label={label}>
        {items.map((it) => (
          <Chip key={it.id} it={it} />
        ))}
      </div>
      {scroll.right ? (
        <button type="button" className="unread-bar-arrow right" aria-label="往右看更多" onClick={() => scroll.by(1)}>
          ▸
        </button>
      ) : null}
    </div>
  )
}

export function UnreadChip() {
  const bots = useStore((s) => s.bots)
  const botUnread = useStore((s) => s.botUnread)
  const runs = useStore((s) => s.runs)
  const selectedBotId = useStore((s) => s.selectedBotId)
  // 側欄收起來的 bot 不畫晶片：點下去會選到一顆側欄裡找不到的 bot。
  const hiddenBotIds = useStore((s) => s.hiddenBotIds)
  const selectBot = useStore((s) => s.selectBot)
  const supervisorProjectId = useStore((s) => s.supervisorProjectId)
  // `selectBot` 會同步清未讀，不記住的話晶片會在手指底下消失。
  const keptId = keepSelectedRow(selectedBotId, botUnread)

  const narrow = useMediaQuery(NARROW_QUERY)

  const items = useMemo(() => {
    const tracked = (b: Bot) => chipTracked(b, supervisorProjectId)
    const waitsKids = (id: string) =>
      bots.some((b) => b.parent_bot_id === id && (runs[b.id]?.agent_status === 'working' || runs[b.id]?.agent_status === 'blocked'))
    const hidden = new Set(hiddenBotIds)
    const out: ChipItem[] = []
    for (const b of bots) {
      if (b.pending || hidden.has(b.id)) continue
      // 釘選是使用者自己指定的，不受排除規則影響。
      const pinned = b.primary
      const n = botUnread[b.id] ?? 0
      const status = runs[b.id]?.agent_status
      const needsReply = status === 'blocked'
      const kids = !needsReply && waitsKids(b.id)
      const current = b.id === selectedBotId
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
    if (narrow) return out
    return out.map((it, i) => ({ it, i })).sort((a, b) => a.it.rank - b.it.rank || a.i - b.i).map((x) => x.it)
  }, [supervisorProjectId, botUnread, bots, hiddenBotIds, keptId, narrow, runs, selectBot, selectedBotId])

  const barRef = useRef<HTMLDivElement | null>(null)
  const [expanded, setExpanded] = useState(false)
  const { hidden, clipPx } = useOverflowRows(barRef, !narrow && !expanded, items.length)

  if (items.length === 0) return null
  if (narrow) {
    return (
      <>
        <ScrollRow items={items.filter((it) => it.pinned)} label="主力 bot" selectedBotId={selectedBotId} />
        <ScrollRow items={items.filter((it) => !it.pinned)} label="在跑或剛完成的 bot" selectedBotId={selectedBotId} />
      </>
    )
  }
  const clipped = !expanded && hidden > 0
  return (
    <div className="unread-bar-wrap">
      <div
        className={`unread-bar${clipped ? ' clipped' : ''}${expanded ? ' expanded' : ''}`}
        style={clipped && clipPx > 0 ? { maxHeight: clipPx } : undefined}
        ref={barRef}
        role="status"
        aria-live="polite"
      >
        {[...items.filter((it) => it.pinned), ...items.filter((it) => !it.pinned)].map((it, i, all) => (
          <Fragment key={it.id}>
            {/* 主力與非主力之間強制換排。 */}
            {!it.pinned && i > 0 && all[i - 1].pinned ? <span className="unread-row-break" aria-hidden="true" /> : null}
            <Chip it={it} />
          </Fragment>
        ))}
      </div>
      {hidden > 0 || expanded ? (
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

/**
 * 兩行裝不下的顆數（依 `offsetTop` 分行），以及裁在第二行底下的高度。永遠全畫、CSS 裁切，不切陣列——render 依賴測量會震盪。
 * 裁切高度用量的：寫死 46px 在晶片加了星號與徽章變高之後，第二行被切一半（2026-09-16 使用者截圖）。
 */
function useOverflowRows(barRef: RefObject<HTMLDivElement | null>, active: boolean, count: number): { hidden: number; clipPx: number } {
  const [hidden, setHidden] = useState(0)
  const [clipPx, setClipPx] = useState(0)
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
    // 值沒變就不寫 state，否則 layout effect 每幀重繪。
    const n = rows.length <= 2 ? 0 : chips.filter((c) => c.offsetTop > cut).length
    setHidden((prev) => (prev === n ? prev : n))
    // 第二行最低那顆的底緣＋下內距：裁切改變的是 bar 自己的高，不影響晶片的 offsetTop，所以量得穩。
    let px = 0
    if (n > 0) {
      const top = bar.getBoundingClientRect().top
      const bottom = Math.max(...chips.filter((c) => c.offsetTop <= cut).map((c) => c.getBoundingClientRect().bottom))
      px = Math.ceil(bottom - top + parseFloat(getComputedStyle(bar).paddingBottom || '0'))
    }
    setClipPx((prev) => (prev === px ? prev : px))
  }, [active, barRef])
  useLayoutEffect(measure, [measure, count])
  useEffect(() => {
    const bar = barRef.current
    if (!bar || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(measure)
    ro.observe(bar)
    return () => ro.disconnect()
  }, [barRef, measure])
  return { hidden, clipPx }
}

/** 手機橫捲：滾輪映射成橫捲；只在真的捲得動時 `preventDefault`，否則整頁被鎖住。 */
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
    // 原生非 passive 掛：React onWheel 是 passive，preventDefault 無效。
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

/** 把 `current` 捲進畫面。不用 `scrollIntoView`（會連帶捲祖先、手機推走 `.app`）；看得見就不動。 */
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
 * 被選走前一刻有未讀就回它的 id，讓它留在列上。
 * 記在模組層：`<ChatPanel key={botId}>` 換 bot 會重新掛載，元件內記憶會被清空。
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
