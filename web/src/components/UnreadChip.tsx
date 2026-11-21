import { msg, tr, useT, type TFn } from "../lib/i18n"
import { chipStateText, kidsText, KEEP_WARM_REPLIED_TEXT } from '../lib/chipStateText'
import { keepWarmReplied } from '../lib/keepWarm'
import { CAPACITY_LABEL, modelAtCapacity } from '../lib/modelCapacity'
import './modelCapacity.css'
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type RefObject } from 'react'
import type { Bot, BotLimitHit } from '../api/types'
import { limitHitWhen } from '../lib/limitHitWhen'
import { KIND_LABEL } from './kindMeta'
import { useMediaQuery } from '../hooks/useMediaQuery'
import { clipAfterRows, layoutBoxes, lineBudget, moreTitle } from '../lib/chipOverflow'
import { chipTracked } from '../lib/supervisorProject'
import { requestBotToTop } from '../lib/scrollBot'
import { loadPinGridRows, pinGridLayout, pinGridRowsNeeded, rowsFromDrag, savePinGridRows, sortPinned } from '../lib/pinnedOrder'
import { useChipFlip } from './useChipFlip'
import { useShallow } from 'zustand/react/shallow'
import { botLamp, orderedBotIds, useStore } from '../store/store'
import { useBotLamp } from '../hooks/useBotLamp'
import { StatusLamp } from './StatusLamp'
import type { Lamp } from '../api/types'
import { usePinnedDrag, type PinnedDnd } from './usePinnedDrag'
import { apiChipCache, chipCache, NO_WARM_TEXT, type CacheState } from '../lib/cacheClock'
import { getApiBotCache, refreshApiBotUsage, useApiBotCacheSub } from '../hooks/useApiTurnCosts'
import { useCacheTick } from '../hooks/useCacheTick'
import { ChipLegend } from './ChipLegend'
import './unreadChip.css'
import './cacheClock.css'
import './keepWarmChip.css'
import { FrozenBadge } from './FrozenBadge'
import { useUserTimeZone } from '../lib/userTime'

const NARROW_QUERY = '(max-width: 720px)'

const PIN_HINT_ID = 'unread-pin-hint'
const PIN_HINT = msg('主力順序是固定的：可以拖曳晶片重排，鍵盤按 Ctrl 加左右方向鍵移動一格，觸控請長按後拖。')

type GroupName = 'pinned' | 'others'
const GROUPS: { name: GroupName; pinned: boolean }[] = [
  { name: 'pinned', pinned: true },
  { name: 'others', pinned: false },
]

interface ChipItem {
  id: string
  name: string
  go: () => void
  position: number
  index: number
  current: boolean
  pinned: boolean
  unread: number
  needsReply: boolean
  atCapacity: boolean
  waitsKids: boolean
  kidsRunning: number
  working: boolean
  title: string
  cache: CacheState | null
  codexPrimary?: boolean
  codexUsage: number | null
  codexUsageTitle: string | null
  keepWarmReplied: boolean
  noWarm: boolean
  limitTitle: string | null
}

function limitTitleOf(kind: Bot['kind'], hit: BotLimitHit | null | undefined, now: number, t: TFn): string | null {
  if (!hit) return null
  const until = hit.until ? Date.parse(hit.until) : NaN
  if (!Number.isNaN(until) && until <= now) return null
  const when = limitHitWhen(hit.until)
  const detail = when ? t('，{when} 恢復', { when }) : t('，恢復時間 CLI 沒寫')
  return t('{kind} 已達用量上限{detail}', { kind: KIND_LABEL[kind], detail })
}

function KidsBadge({ n }: { n: number }) {
  return (
    <span className="unread-chip-kids" aria-hidden="true" title={kidsText(n)}>
      <svg viewBox="0 0 12 12" width="11" height="11" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round">
        <circle cx="3" cy="2.5" r="1.3" />
        <circle cx="3" cy="9.5" r="1.3" />
        <circle cx="9" cy="6" r="1.3" />
        <path d="M3 3.8v4.4M3 6h4.7" />
      </svg>
      {n > 1 ? <span className="unread-chip-kids-n">{n > 9 ? '9+' : n}</span> : null}
    </span>
  )
}

const LAMP_SHOWN = new Set<Lamp>(['working', 'blocked', 'unknown', 'disconnected'])

function ChipLamp({ id }: { id: string }) {
  const { lamp, background, kids } = useBotLamp(id)
  if (lamp === 'working' || (lamp === 'idle' && kids > 0)) return null
  return LAMP_SHOWN.has(lamp) || background > 0 || kids > 0 ? <StatusLamp lamp={lamp} background={background} kids={kids} /> : null
}

function Chip({ it, dnd, lamp }: { it: ChipItem; dnd?: PinnedDnd; lamp?: boolean }) {
  const drag = it.pinned ? dnd : undefined
  const dragging = drag?.dragId != null && drag.dragId !== it.id
  const shifted = dragging && drag.shifted.includes(it.id)
  const mark = dragging && drag.before !== undefined ? (drag.after === it.id ? ' drop-after' : drag.after == null && drag.before === it.id ? ' drop-before' : '') : ''
  const cache = it.cache && it.cache.level !== 'cold' ? it.cache : null
  const fill = it.codexPrimary ? ({ '--cache-fill': `${((it.codexUsage ?? 0) * 100).toFixed(1)}%` } as CSSProperties) : cache ? ({ '--cache-fill': `${(cache.frac * 100).toFixed(1)}%` } as CSSProperties) : undefined
  const cacheCls = it.codexPrimary ? ' cache-codex' : cache ? ` cache-${cache.level}` : ''
  return (
    <button
      type="button"
      className={`${chipClass(it)}${cacheCls}${it.keepWarmReplied ? ' keep-warm-replied' : ''}${drag?.dragId === it.id ? ' dragging' : ''}${shifted ? ' shifted' : ''}${mark}`}
      title={[it.title, it.limitTitle, it.codexUsageTitle, it.cache?.title ?? (it.noWarm ? NO_WARM_TEXT : null), it.keepWarmReplied ? KEEP_WARM_REPLIED_TEXT : null].filter(Boolean).join('\n')}
      data-bot-id={it.pinned ? it.id : undefined}
      style={
        drag?.dragId === it.id
          ? { ...fill, transform: `translate(${drag.offset.x}px, ${drag.offset.y}px) scale(1.06)` }
          : dragging
            ? ({ ...fill, '--drop-w': `${drag.gap}px`, transform: shifted ? `translateX(${drag.gap}px)` : undefined } as CSSProperties)
            : fill
      }
      aria-current={it.current ? 'true' : undefined}
      aria-describedby={drag ? PIN_HINT_ID : undefined}
      onClick={() => {
        if (drag?.consumeClick()) return
        it.go()
      }}
      onPointerDown={drag ? (e) => drag.onPointerDown(e, it.id) : undefined}
      onKeyDown={drag ? (e) => drag.onKeyDown(e, it.id) : undefined}
    >
      {it.working || it.waitsKids ? <span className="unread-chip-working-ring" aria-hidden="true" /> : null}
      {lamp ? <ChipLamp id={it.id} /> : null}
      <span className="unread-chip-name">{it.name}</span>
      {it.noWarm ? <FrozenBadge /> : null}
      {it.keepWarmReplied ? (
        <span className="unread-chip-warm" aria-hidden="true">
          ♨︎
        </span>
      ) : null}
      {it.unread > 0 ? <span className="unread-chip-n" aria-hidden="true">{it.unread > 99 ? '99+' : it.unread}</span> : null}
      {it.needsReply || it.atCapacity ? (
        <span className="unread-chip-dot" aria-hidden="true" />
      ) : null}
      {it.limitTitle ? <span className="unread-chip-limit" aria-hidden="true" /> : null}
      {it.kidsRunning > 0 ? <KidsBadge n={it.kidsRunning} /> : null}
      {}
      <span className="sr-only">{[chipStateText({ ...it, kids: it.kidsRunning }), it.limitTitle, it.codexUsageTitle].filter(Boolean).join(' · ')}</span>
    </button>
  )
}

function ScrollRow({ items, label, selectedBotId, dnd }: { items: ChipItem[]; label: string; selectedBotId: string | null; dnd?: PinnedDnd }) {
  if (items.length === 0) return null
  return <ScrollRowBar items={items} label={label} selectedBotId={selectedBotId} dnd={dnd} />
}

function ScrollRowBar({ items, label, selectedBotId, dnd }: { items: ChipItem[]; label: string; selectedBotId: string | null; dnd?: PinnedDnd }) {
  const t = useT()
  const barRef = useRef<HTMLDivElement | null>(null)
  const scroll = useHorizontalScroll(barRef, true)
  useScrollCurrentIntoView(barRef, `${selectedBotId}/${items.length}`)
  useChipFlip(barRef, dnd?.dragId != null)
  return (
    <div className={`unread-bar-wrap row${scroll.left ? ' can-left' : ''}${scroll.right ? ' can-right' : ''}`}>
      {scroll.left ? (
        <button type="button" className="unread-bar-arrow left" aria-label={t("往左看更多")} onClick={() => scroll.by(-1)}>
          ◂
        </button>
      ) : null}
      <div className="unread-bar" ref={barRef} role="status" aria-live="polite" aria-label={label}>
        {items.map((it) => (
          <Chip key={it.id} it={it} dnd={dnd} />
        ))}
        {dnd ? <span className="sr-only" aria-live="polite">{dnd.announce}</span> : null}
      </div>
      {scroll.right ? (
        <button type="button" className="unread-bar-arrow right" aria-label={t("往右看更多")} onClick={() => scroll.by(1)}>
          ▸
        </button>
      ) : null}
    </div>
  )
}

export function UnreadChip() {
  const t = useT()
  useUserTimeZone()
  const bots = useStore((s) => s.bots)
  const projects = useStore((s) => s.projects)
  const botUnread = useStore((s) => s.botUnread)
  const runs = useStore((s) => s.runs)
  const lamps = useStore(useShallow((s) => Object.fromEntries(s.bots.map((b) => [b.id, botLamp(s, b.id)])) as Record<string, Lamp>))
  const selectedBotId = useStore((s) => s.selectedBotId)
  const hiddenBotIds = useStore((s) => s.hiddenBotIds)
  const selectBot = useStore((s) => s.selectBot)
  const supervisorProjectId = useStore((s) => s.supervisorProjectId)
  const keptId = keepSelectedRow(selectedBotId, botUnread)

  const narrow = useMediaQuery(NARROW_QUERY)
  const pinnedApiBotIds = useMemo(
    () => bots.filter((b) => b.primary && b.kind === 'claude-api').map((b) => b.id),
    [bots],
  )
  const apiCacheVer = useApiBotCacheSub(pinnedApiBotIds)

  const prevLamps = useRef<Record<string, Lamp>>({})
  useEffect(() => {
    for (const id of pinnedApiBotIds) {
      const cur = lamps[id]
      const prev = prevLamps.current[id]
      if (prev !== undefined && prev !== cur) {
        refreshApiBotUsage(id)
      }
    }
    prevLamps.current = { ...lamps }
  }, [lamps, pinnedApiBotIds])

  const [primaryRed, setPrimaryRed] = useState(false)
  const now = useCacheTick(bots.some((b) => b.primary), primaryRed)
  const redNow = bots.some((b) => {
    if (!b.primary) return false
    const c = b.kind === 'claude-api'
      ? apiChipCache(getApiBotCache(b.id), now, lamps[b.id] === 'working')
      : b.kind === 'openai-api' ? null : chipCache(runs[b.id], now)
    return c?.level === 'low'
  })
  if (redNow !== primaryRed) setPrimaryRed(redNow)

  const items = useMemo(() => {
    void apiCacheVer
    const tracked = (b: Bot) => chipTracked(b, supervisorProjectId)
    const kidsRunning = (id: string) =>
      bots.filter((b) => b.parent_bot_id === id && (lamps[b.id] === 'working' || lamps[b.id] === 'blocked')).length
    const hidden = new Set(hiddenBotIds)
    const out: ChipItem[] = []
    const projectInitials = new Map(projects.map((p) => [p.id, Array.from((p.label ?? '').trim())[0]?.toUpperCase()]))
    for (const b of bots) {
      if (b.pending || hidden.has(b.id)) continue
      const pinned = b.primary
      const n = botUnread[b.id] ?? 0
      const lamp = lamps[b.id]
      const needsReply = lamp === 'blocked'
      const atCapacity = !needsReply && modelAtCapacity(runs[b.id])
      const working = lamp === 'working'
      const nKids = kidsRunning(b.id)
      const kids = !needsReply && nKids > 0
      const current = b.id === selectedBotId
      const show = pinned || (tracked(b) && (n > 0 || b.id === keptId || working || needsReply || atCapacity))
      if (!show) continue
      const pc = runs[b.id]?.prompt_cache
      const used = pc?.context_used_tokens
      const size = pc?.context_size
      const codexUsage = pinned && b.kind === 'codex' && used != null && size != null && Number.isFinite(used) && Number.isFinite(size) && used >= 0 && size > 0 ? Math.max(0, Math.min(1, used / size)) : null
      const codexUsageTitle = pinned && b.kind === 'codex' ? `context · ${codexUsage === null ? t('未知') : `${(codexUsage * 100).toFixed(1)}%`}` : null
      out.push({
        id: b.id,
        name: pinned && projectInitials.get(b.project_id) ? `${projectInitials.get(b.project_id)}.${b.name}` : b.name,
        go: () => {
          selectBot(b.id)
          if (pinned) requestBotToTop(b.id)
        },
        position: b.primary_position,
        index: out.length,
        current,
        pinned,
        unread: n,
        needsReply,
        atCapacity,
        waitsKids: kids,
        kidsRunning: nKids,
        working,
        title: botTitle(b.name, pinned, n, needsReply, kids, current, t, atCapacity) + (nKids > 0 ? t('（{kids}）', { kids: kidsText(nKids) }) : ''),
        keepWarmReplied: pinned && keepWarmReplied(runs[b.id], now),
        cache: pinned
          ? b.kind === 'claude-api'
            ? apiChipCache(getApiBotCache(b.id), now, working)
            : b.kind === 'openai-api' ? null : chipCache(runs[b.id], now, working)
          : null,
        codexPrimary: pinned && b.kind === 'codex',
        codexUsage,
        codexUsageTitle,
        noWarm: pinned && runs[b.id]?.keep_warm_skip === true,
        limitTitle: pinned ? limitTitleOf(b.kind, b.limit_hit, now, t) : null,
      })
    }
    return out
  }, [supervisorProjectId, botUnread, bots, projects, hiddenBotIds, keptId, lamps, runs, selectBot, selectedBotId, now, t, apiCacheVer])

  const pinnedItems = useMemo(() => sortPinned(items.filter((it) => it.pinned)), [items])
  const sidebarRank = useStore((s) => s.botOrder)
  const projectOrder = useStore((s) => s.projectOrder)
  const otherItems = useMemo(() => {
    const rank = new Map(orderedBotIds({ projects, projectOrder, bots, botOrder: sidebarRank }).map((id, i) => [id, i]))
    return items.filter((it) => !it.pinned).sort((a, b) => (rank.get(a.id) ?? Infinity) - (rank.get(b.id) ?? Infinity) || a.index - b.index)
  }, [items, projects, projectOrder, bots, sidebarRank])

  const movePrimary = useStore((s) => s.movePrimary)
  const fullPinned = useMemo(
    () => sortPinned(bots.filter((b) => b.primary && !b.pending).map((b, i) => ({ id: b.id, position: b.primary_position, index: i }))).map((x) => x.id),
    [bots],
  )
  const [pinExpanded, setPinExpanded] = useState(false)
  const [pinRows, setPinRows] = useState(loadPinGridRows)
  const pinLayout = pinGridLayout(pinnedItems.length, pinExpanded, pinRows)
  const shownPinned = useMemo(
    () => (narrow ? pinnedItems.slice(0, pinLayout.shown) : pinnedItems),
    [narrow, pinnedItems, pinLayout.shown],
  )
  const hiddenPinned = useMemo(() => (narrow ? pinnedItems.slice(pinLayout.shown) : []), [narrow, pinnedItems, pinLayout.shown])
  const visiblePinned = useMemo(() => shownPinned.map((it) => it.id), [shownPinned])
  const names = useMemo(() => Object.fromEntries(pinnedItems.map((it) => [it.id, it.name])), [pinnedItems])
  const [legend, setLegend] = useState(false)
  const dnd = usePinnedDrag(fullPinned, visiblePinned, names, movePrimary)
  const ordered = useMemo(() => [...pinnedItems, ...otherItems], [pinnedItems, otherItems])
  const barRef = useRef<HTMLDivElement | null>(null)
  const [expanded, setExpanded] = useState(false)
  const { hiddenItems, clipPx } = useOverflowChips(barRef, !narrow && !expanded, ordered)
  useChipFlip(barRef, dnd.dragId != null)
  const hidden = hiddenItems.length

  if (items.length === 0) return null
  if (narrow) {
    return (
      <>
        <PinHint />
        <PinGrid
          items={shownPinned}
          dnd={dnd}
          hidden={hiddenPinned}
          collapsible={pinLayout.collapsible}
          onToggle={() => setPinExpanded((v) => !v)}
          rows={pinRows}
          total={pinnedItems.length}
          onRows={(n, done) => {
            setPinRows(n)
            if (done) savePinGridRows(n)
          }}
          resizable={!pinExpanded}
        />
        <ScrollRow items={otherItems} label={t("在跑或剛完成的 bot")} selectedBotId={selectedBotId} />
        {legend ? <ChipLegend onClose={() => setLegend(false)} /> : null}
      </>
    )
  }
  return (
    <div className="unread-bar-wrap">
      <PinHint />
      <div className={`unread-bar${expanded ? ' expanded' : ''}`} ref={barRef} role="status" aria-live="polite">
        {GROUPS.map(({ name, pinned }) => {
          const group = pinned ? pinnedItems : otherItems
          if (group.length === 0) return null
          return (
            <div
              key={name}
              className="unread-group"
              data-group={name}
              style={!expanded && clipPx[name] > 0 ? { maxHeight: clipPx[name], overflow: 'hidden' } : undefined}
            >
              {}
              {!pinned && pinnedItems.length > 0 ? <span className="unread-group-label">{t('進行中')}</span> : null}
              {group.map((it) => (
                <Chip key={it.id} it={it} dnd={pinned ? dnd : undefined} />
              ))}
              {pinned ? <span className="sr-only" aria-live="polite">{dnd.announce}</span> : null}
            </div>
          )
        })}
      </div>
      {hidden > 0 || expanded ? (
        <button
          type="button"
          className="unread-bar-more"
          aria-expanded={expanded}
          title={expanded ? t('收合') : moreTitle(hiddenItems)}
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded ? t('收合') : `+${hidden}`}
        </button>
      ) : null}
      {}
      <button type="button" className="unread-bar-help" aria-label={t("顏色代表什麼？")} title={t("顏色代表什麼？")} onClick={() => setLegend(true)}>
        ?
      </button>
      {legend ? <ChipLegend onClose={() => setLegend(false)} /> : null}
    </div>
  )
}

function PinGrid({
  items,
  dnd,
  hidden = [],
  collapsible = false,
  onToggle,
  rows,
  total = 0,
  onRows,
  resizable = false,
}: {
  items: ChipItem[]
  dnd: PinnedDnd
  hidden?: ChipItem[]
  collapsible?: boolean
  onToggle?: () => void
  rows?: number
  total?: number
  onRows?: (n: number, done: boolean) => void
  resizable?: boolean
}) {
  const t = useT()
  const gridRef = useRef<HTMLDivElement | null>(null)
  const drag = useRef<{ y: number; rows: number; rowPx: number } | null>(null)
  if (items.length === 0) return null
  const canResize = resizable && onRows && rows !== undefined && pinGridRowsNeeded(total) > 1
  const rowPx = () => {
    const chip = gridRef.current?.querySelector<HTMLElement>('.unread-chip')
    const gap = gridRef.current ? parseFloat(getComputedStyle(gridRef.current).rowGap) || 0 : 0
    return chip ? chip.offsetHeight + gap : 0
  }
  const moreState = hidden.some((h) => h.needsReply) ? ' needs-reply' : hidden.some((h) => h.atCapacity) ? ' at-capacity' : hidden.some((h) => h.unread > 0) ? ' unread' : ''
  return (
    <div className={`unread-bar-wrap row${canResize ? ' pin-resizable' : ''}`}>
      <div className="unread-pin-grid" ref={gridRef} role="status" aria-live="polite" aria-label={t("主力 bot")}>
        {items.map((it) => (
          <Chip key={it.id} it={it} dnd={dnd} lamp />
        ))}
        {hidden.length > 0 ? (
          <button
            type="button"
            className={`unread-chip pin-more${moreState}`}
            title={moreTitle(hidden)}
            aria-label={t('{title}：{names}', { title: moreTitle(hidden), names: hidden.map((h) => h.name).join(' · ') })}
            aria-expanded={false}
            onClick={onToggle}
          >
            <span className="unread-chip-name">+{hidden.length}</span>
          </button>
        ) : collapsible ? (
          <button type="button" className="unread-chip pin-more" aria-expanded title={t('收合成 {n} 排', { n: rows ?? 3 })} onClick={onToggle}>
            <span className="unread-chip-name">{t("收合")}</span>
          </button>
        ) : null}
        <span className="sr-only" aria-live="polite">
          {dnd.announce}
        </span>
      </div>
      {canResize ? (
        <div
          className="pin-rows-handle"
          role="slider"
          tabIndex={0}
          aria-label={t("主力區最多幾排（上下拖）")}
          aria-valuemin={1}
          aria-valuemax={pinGridRowsNeeded(total)}
          aria-valuenow={Math.min(rows, pinGridRowsNeeded(total))}
          aria-valuetext={t('{n} 排', { n: Math.min(rows, pinGridRowsNeeded(total)) })}
          title={t("上下拖，調整主力區最多幾排")}
          onPointerDown={(e) => {
            e.currentTarget.setPointerCapture(e.pointerId)
            drag.current = { y: e.clientY, rows: Math.min(rows, pinGridRowsNeeded(total)), rowPx: rowPx() }
          }}
          onPointerMove={(e) => {
            const d = drag.current
            if (!d) return
            const n = rowsFromDrag(d.rows, e.clientY - d.y, d.rowPx, total)
            if (n !== rows) onRows(n, false)
          }}
          onPointerUp={(e) => {
            const d = drag.current
            drag.current = null
            if (d) onRows(rowsFromDrag(d.rows, e.clientY - d.y, d.rowPx, total), true)
          }}
          onPointerCancel={() => {
            drag.current = null
          }}
          onKeyDown={(e) => {
            const cur = Math.min(rows, pinGridRowsNeeded(total))
            const next = e.key === 'ArrowDown' ? cur + 1 : e.key === 'ArrowUp' ? cur - 1 : cur
            const n = Math.min(pinGridRowsNeeded(total), Math.max(1, next))
            if (n !== cur) {
              e.preventDefault()
              onRows(n, true)
            }
          }}
        >
          <svg viewBox="0 0 12 12" width="12" height="12" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
            <path d="M3.5 4.5 6 2l2.5 2.5M3.5 7.5 6 10l2.5-2.5" />
          </svg>
        </div>
      ) : null}
    </div>
  )
}

function PinHint() {
  return (
    <span id={PIN_HINT_ID} className="sr-only">
      {tr(PIN_HINT)}
    </span>
  )
}

function chipClass(it: ChipItem): string {
  const state = it.needsReply ? ' needs-reply' : it.atCapacity ? ' at-capacity' : it.unread > 0 ? ' unread' : it.waitsKids ? ' waits-kids' : it.working ? ' working' : ''
  return `unread-chip${it.pinned ? ' pinned' : ''}${state}${it.current ? ' current' : ''}`
}

function botTitle(name: string, pinned: boolean, n: number, needsReply: boolean, kids: boolean, current: boolean, t: TFn, atCapacity = false): string {
  const who = pinned ? t('{name}（主要執行的 bot）', { name }) : name
  const tail = current ? t('你正在看的就是它') : t('點一下跳過去')
  if (needsReply) return t('{who} 停在一個要你回答的提示上。{action}', { who, action: current ? tail : t('點一下過去回答') })
  if (atCapacity) return t('{who}：{label}，這一回合沒跑（不是卡住）。{tail}', { who, label: CAPACITY_LABEL, tail })
  if (n > 0) return t('{who} 有 {n} 個回合已完成、還沒看過。{tail}', { who, n, tail })
  if (kids) return t('{who} 在等子 agent 完成。{tail}', { who, tail })
  return t('{who}。{tail}', { who, tail })
}

function useOverflowChips(
  barRef: RefObject<HTMLDivElement | null>,
  active: boolean,
  ordered: ChipItem[],
): { hiddenItems: ChipItem[]; clipPx: Record<GroupName, number> } {
  const [cut, setCut] = useState({ ids: '', clip: '0 0' })
  const measure = useCallback(() => {
    const bar = barRef.current
    if (!bar || !active) {
      setCut((prev) => (prev.ids === '' && prev.clip === '0 0' ? prev : { ids: '', clip: '0 0' }))
      return
    }
    const groups = [...bar.querySelectorAll<HTMLElement>('.unread-group')]
    const has = (name: GroupName) => groups.some((g) => g.dataset.group === name)
    const budget = lineBudget(has('pinned'))
    const ids: string[] = []
    const px: Record<string, number> = { pinned: 0, others: 0 }
    let at = 0
    for (const g of groups) {
      const chips = [...g.querySelectorAll<HTMLElement>('.unread-chip')]
      const cut = clipAfterRows(layoutBoxes(chips, g.offsetTop), g.dataset.group === 'pinned' ? budget.pinned : budget.others)
      for (const i of cut.hidden) ids.push(ordered[at + i]?.id ?? '')
      if (cut.hidden.length > 0) {
        px[g.dataset.group ?? 'others'] = Math.ceil(cut.visibleBottom + parseFloat(getComputedStyle(g).paddingBottom || '0'))
      }
      at += chips.length
    }
    const next = { ids: ids.filter(Boolean).join(' '), clip: `${px.pinned} ${px.others}` }
    setCut((prev) => (prev.ids === next.ids && prev.clip === next.clip ? prev : next))
  }, [active, barRef, ordered])
  useLayoutEffect(measure, [measure])
  useEffect(() => {
    const bar = barRef.current
    if (!bar || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(measure)
    ro.observe(bar)
    return () => ro.disconnect()
  }, [barRef, measure])
  const hiddenItems = useMemo(() => {
    const ids = new Set(cut.ids.split(' '))
    return ordered.filter((it) => ids.has(it.id))
  }, [cut.ids, ordered])
  const clipPx = useMemo(() => {
    const [pinned, others] = cut.clip.split(' ').map(Number)
    return { pinned, others }
  }, [cut.clip])
  return { hiddenItems, clipPx }
}

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
  useLayoutEffect(() => {
    sync()
  })
  useEffect(() => {
    const bar = barRef.current
    if (!bar) return
    sync()
    const onWheel = (e: WheelEvent) => {
      if (!active) return
      const max = bar.scrollWidth - bar.clientWidth
      if (max <= 4 || Math.abs(e.deltaY) <= Math.abs(e.deltaX)) return
      const next = Math.min(max, Math.max(0, bar.scrollLeft + e.deltaY))
      if (next === bar.scrollLeft) return
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
