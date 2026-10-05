/**
 * 標題列下面一列：等你、在跑、或釘選的 bot；一顆 bot 一顆晶片，狀態疊在同一顆上。
 *
 * 排序＝緊急度（2026-09-12 使用者／AGM，推翻 9/11 分組）：needs-reply → 未讀 → waits-kids → current → 其餘；
 * 同級照 `bots` 順序。不分組標籤：分組會把第 9 顆 needs-reply 擠出視野（使用者 2026-09-12 錯過 blocked bot）。
 * 手機不排序（2026-09-13 使用者：「手機版星號列不要任意改變順序」）：單行橫捲靠位置肌肉記憶。
 * 版面（同日）：桌機換行、有行數上限（主力三行，其餘一到兩行，2026-09-23）、`+N` 展開；手機單行橫捲要看得出能捲（陰影＋◂ ▸、滾輪映射、scroll-snap）。
 * 兩排（2026-09-15 使用者：「非標主力之現執行中與剛完成的 bot 要出現在主力的下一排」）：★ 主力一排，
 * 沒釘的（在跑、剛跑完、要回答）一定換到下一排；各排裡照上面的排序。桌機收合時主力最多三行、
 * 其餘那組照舊一行（只有它時兩行），主力再多也擠不掉沒釘的「要你回答」（`lib/chipOverflow.ts`）。手機單行橫捲放不下兩排，照同樣分組
 * 分成上下兩排，各自橫捲（2026-09-16 使用者：「已完成放下一排，方便我點選」）。
 * 排除（2026-09-10 使用者）：AGM 總管專案不算（例行 loop 會洗版），但 ★ 釘選不受影響；
 * 認 `GET /api/supervisor` 的 `project_id` 不認名字（2026-09-13 已從 `AGM` 改名 `AGM-DM-GRUP`）。
 */
import { chipStateText, kidsText, KEEP_WARM_REPLIED_TEXT } from '../lib/chipStateText'
import { keepWarmReplied } from '../lib/keepWarm'
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type RefObject } from 'react'
import type { Bot } from '../api/types'
import { useMediaQuery } from '../hooks/useMediaQuery'
import { clipAfterRows, layoutBoxes, lineBudget, moreTitle } from '../lib/chipOverflow'
import { chipTracked } from '../lib/supervisorProject'
import { loadPinGridRows, pinGridLayout, pinGridRowsNeeded, rowsFromDrag, savePinGridRows, sortPinned } from '../lib/pinnedOrder'
import { useChipFlip } from './useChipFlip'
import { orderedBotIds, useStore } from '../store/store'
import { useBotLamp } from '../hooks/useBotLamp'
import { StatusLamp } from './StatusLamp'
import type { Lamp } from '../api/types'
import { usePinnedDrag, type PinnedDnd } from './usePinnedDrag'
import { cacheState, type CacheState } from '../lib/cacheClock'
import { useCacheTick } from '../hooks/useCacheTick'
import { ChipLegend } from './ChipLegend'
import { BotStatusCard, type ChipHints } from './BotStatusCard'
import './unreadChip.css'
import './cacheClock.css'
import './keepWarmChip.css'

/** 與 `unreadChip.css` 斷點同值。 */
const NARROW_QUERY = '(max-width: 720px)'

/** 主力晶片的操作說明（`aria-describedby` 指到它）；畫面上看不到。 */
const PIN_HINT_ID = 'unread-pin-hint'
const PIN_HINT = '主力順序是固定的：可以拖曳晶片重排，鍵盤按 Ctrl 加左右方向鍵移動一格，觸控請長按後拖。'

/** 桌機的兩組：★ 主力一組、其餘（在跑、剛跑完、要回答）一組，各自換行、各自裁切。 */
type GroupName = 'pinned' | 'others'
const GROUPS: { name: GroupName; pinned: boolean }[] = [
  { name: 'pinned', pinned: true },
  { name: 'others', pinned: false },
]

interface ChipItem {
  id: string
  name: string
  go: () => void
  /** 主力組排序用（#344）：`primary_position`＋原順序。 */
  position: number
  index: number
  current: boolean
  pinned: boolean
  unread: number
  needsReply: boolean
  waitsKids: boolean
  /** parent_bot_id 指到這顆、且 working／blocked 的子 bot 數（#344 補充 4）；不受自己狀態影響。 */
  kidsRunning: number
  working: boolean
  title: string
  /** 主力晶片的快取倒數（`lib/cacheClock.ts`）；grok、沒紀錄、非主力是 `null`。 */
  cache: CacheState | null
  /** 主力：保溫回覆到了、使用者還沒送新 prompt → 晶片框換色（`keepWarmChip.css`）。 */
  keepWarmReplied: boolean
}

/** 子 agent 在跑：分叉圖示（形狀跟燈／點不同）＋多顆時的數字；文字說明在 `title` 與 sr-only。 */
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

/** 手機主力晶片只畫「值得注意」的狀態；idle（常態）、離線、啟動／停止中都不畫，全亮是雜訊（#344 補充 3）。 */
const LAMP_SHOWN = new Set<Lamp>(['working', 'blocked', 'unknown', 'disconnected'])

/** 側欄同一顆燈（`botLamp`）：working 會脈動、blocked 紅、斷線灰。 */
function ChipLamp({ id }: { id: string }) {
  const { lamp, background, kids } = useBotLamp(id)
  // idle 平常不畫；子 agent 還在跑（轉圈）或背景還有工作（一般綠點，#714）才畫，不能看起來像停了。
  return LAMP_SHOWN.has(lamp) || background > 0 || kids > 0 ? <StatusLamp lamp={lamp} background={background} kids={kids} /> : null
}

/** 有滑鼠可以 hover 的裝置（手機的 tap 也會觸發 mouseenter，不能拿來開浮卡）。 */
const canHover = () => typeof window !== 'undefined' && window.matchMedia?.('(hover: hover)').matches === true
/** 滑鼠停多久才開狀態卡：掃過晶片列不該每顆都閃一下。 */
const PEEK_DELAY_MS = 400

function chipHints(it: ChipItem): ChipHints {
  return { current: it.current, unread: it.unread, needsReply: it.needsReply, waitsKids: it.waitsKids, kidsRunning: it.kidsRunning, cacheTitle: it.cache?.title ?? null, keepWarmReplied: it.keepWarmReplied }
}

function Chip({ it, dnd, lamp, onPeek }: { it: ChipItem; dnd?: PinnedDnd; lamp?: boolean; /** 主力晶片：hover 開狀態卡（`null`＝收起）。 */ onPeek?: (id: string | null, at?: DOMRect) => void }) {
  const peekTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const drag = it.pinned ? dnd : undefined
  const dragging = drag?.dragId != null && drag.dragId !== it.id
  const shifted = dragging && drag.shifted.includes(it.id)
  // 落點標示：讓位的那一格（`drop-before`，標示畫在這顆左邊的空位）；落在行尾則畫在那一行最後一顆右邊（`drop-after`）。
  const mark = dragging && drag.before !== undefined ? (drag.after === it.id ? ' drop-after' : drag.after == null && drag.before === it.id ? ' drop-before' : '') : ''
  // 快取倒數畫在底色上（`cacheClock.css`）：已涼不填，回到一般底色。
  const cache = it.cache && it.cache.level !== 'cold' ? it.cache : null
  const fill = cache ? ({ '--cache-fill': `${(cache.frac * 100).toFixed(1)}%` } as CSSProperties) : undefined
  return (
    <button
      type="button"
      className={`${chipClass(it)}${cache ? ` cache-${cache.level}` : ''}${it.keepWarmReplied ? ' keep-warm-replied' : ''}${drag?.dragId === it.id ? ' dragging' : ''}${shifted ? ' shifted' : ''}${mark}`}
      // 主力晶片在能 hover 的裝置上改用狀態卡，不再疊一個瀏覽器原生 tooltip。
      title={onPeek && canHover() ? undefined : [it.title, it.cache?.title, it.keepWarmReplied ? KEEP_WARM_REPLIED_TEXT : null].filter(Boolean).join('\n')}
      onMouseEnter={
        onPeek
          ? (e) => {
              if (!canHover()) return
              const el = e.currentTarget
              peekTimer.current = setTimeout(() => onPeek(it.id, el.getBoundingClientRect()), PEEK_DELAY_MS)
            }
          : undefined
      }
      onMouseLeave={
        onPeek
          ? () => {
              if (peekTimer.current) clearTimeout(peekTimer.current)
              peekTimer.current = null
              onPeek(null)
            }
          : undefined
      }
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
      {lamp ? (
        // 手機主力區（#344）：整區都是釘選的，★ 每顆一樣只佔寬；改放側欄那顆燈號。
        <ChipLamp id={it.id} />
      ) : it.pinned ? (
        <span className="unread-chip-star" aria-hidden="true">
          ★
        </span>
      ) : null}
      <span className="unread-chip-name">{it.name}</span>
      {it.keepWarmReplied ? (
        <span className="unread-chip-warm" aria-hidden="true">
          ♨︎
        </span>
      ) : null}
      {it.unread > 0 ? <span className="unread-chip-n" aria-hidden="true">{it.unread > 99 ? '99+' : it.unread}</span> : null}
      {it.working || it.needsReply || it.waitsKids ? <span className="unread-chip-dot" aria-hidden="true" /> : null}
      {it.kidsRunning > 0 ? <KidsBadge n={it.kidsRunning} /> : null}
      <span className="sr-only">{chipStateText({ ...it, kids: it.kidsRunning })}</span>
    </button>
  )
}

/**
 * 手機的一排：自己橫捲、自己一組箭頭。★ 主力一排、在跑／剛完成的另一排（2026-09-16 使用者：
 * 「已完成放下一排，方便我點選」），不必先把主力捲過去才點得到剛跑完的那顆。
 */
function ScrollRow({ items, label, selectedBotId, dnd }: { items: ChipItem[]; label: string; selectedBotId: string | null; dnd?: PinnedDnd }) {
  // 空的那排整個不掛：捲動 hook 的 effect 只在掛載時讀 `barRef`，空著掛上去讀到 null 就再也不接
  // scroll／wheel／ResizeObserver——之後晶片出現、溢出了也沒有 ◂ ▸（review3 c5 L1）。
  if (items.length === 0) return null
  return <ScrollRowBar items={items} label={label} selectedBotId={selectedBotId} dnd={dnd} />
}

function ScrollRowBar({ items, label, selectedBotId, dnd }: { items: ChipItem[]; label: string; selectedBotId: string | null; dnd?: PinnedDnd }) {
  const barRef = useRef<HTMLDivElement | null>(null)
  const scroll = useHorizontalScroll(barRef, true)
  useScrollCurrentIntoView(barRef, `${selectedBotId}/${items.length}`)
  useChipFlip(barRef, dnd?.dragId != null)
  return (
    <div className={`unread-bar-wrap row${scroll.left ? ' can-left' : ''}${scroll.right ? ' can-right' : ''}`}>
      {scroll.left ? (
        <button type="button" className="unread-bar-arrow left" aria-label="往左看更多" onClick={() => scroll.by(-1)}>
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
  // 快取倒數每 15 秒重算；沒有主力就不跑計時器。
  const now = useCacheTick(bots.some((b) => b.primary))

  const items = useMemo(() => {
    const tracked = (b: Bot) => chipTracked(b, supervisorProjectId)
    const kidsRunning = (id: string) =>
      bots.filter((b) => b.parent_bot_id === id && (runs[b.id]?.agent_status === 'working' || runs[b.id]?.agent_status === 'blocked')).length
    const hidden = new Set(hiddenBotIds)
    const out: ChipItem[] = []
    for (const b of bots) {
      if (b.pending || hidden.has(b.id)) continue
      // 釘選是使用者自己指定的，不受排除規則影響。
      const pinned = b.primary
      const n = botUnread[b.id] ?? 0
      const status = runs[b.id]?.agent_status
      const needsReply = status === 'blocked'
      const nKids = kidsRunning(b.id)
      const kids = !needsReply && nKids > 0
      const current = b.id === selectedBotId
      const show = pinned || (tracked(b) && (n > 0 || b.id === keptId || status === 'working' || needsReply))
      if (!show) continue
      out.push({
        id: b.id,
        name: b.name,
        go: () => selectBot(b.id),
        position: b.primary_position,
        index: out.length,
        current,
        pinned,
        unread: n,
        needsReply,
        waitsKids: kids,
        kidsRunning: nKids,
        working: status === 'working',
        title: botTitle(b.name, pinned, n, needsReply, kids, current) + (nKids > 0 ? `（${kidsText(nKids)}）` : ''),
        keepWarmReplied: pinned && keepWarmReplied(runs[b.id]),
        cache: pinned ? cacheState(runs[b.id]?.last_api_at, runs[b.id]?.cache_ttl_secs, now, status === 'working', runs[b.id]?.cache_kept_warm_at) : null,
      })
    }
    return out
  }, [supervisorProjectId, botUnread, bots, hiddenBotIds, keptId, runs, selectBot, selectedBotId, now])

  // 固定順序（#344）：主力組照 `primary_position`，其餘組照側欄順序；未讀／忙碌／卡住只用顏色與角標表示、不再讓晶片跳位。
  const pinnedItems = useMemo(() => sortPinned(items.filter((it) => it.pinned)), [items])
  const sidebarRank = useStore((s) => s.botOrder)
  const projects = useStore((s) => s.projects)
  const projectOrder = useStore((s) => s.projectOrder)
  const otherItems = useMemo(() => {
    const rank = new Map(orderedBotIds({ projects, projectOrder, bots, botOrder: sidebarRank }).map((id, i) => [id, i]))
    return items.filter((it) => !it.pinned).sort((a, b) => (rank.get(a.id) ?? Infinity) - (rank.get(b.id) ?? Infinity) || a.index - b.index)
  }, [items, projects, projectOrder, bots, sidebarRank])

  // 存回 daemon 用完整的主力順序（含側欄收起來、沒畫成晶片的）。
  const movePrimary = useStore((s) => s.movePrimary)
  const fullPinned = useMemo(
    () => sortPinned(bots.filter((b) => b.primary && !b.pending).map((b, i) => ({ id: b.id, position: b.primary_position, index: i }))).map((x) => x.id),
    [bots],
  )
  // 手機：4 顆一排、收合時預設三排（下緣把手可拖，記在這台裝置）；放不下時最後一格是「+N」，點了展開全部（`pinGridLayout`）。桌機全畫。
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
  // 主力 bot 狀態卡（2026-10-04 使用者）：電腦 hover（`at`＝晶片位置），手機長按一到就開（`at` 為 null，底部彈出；接著移動＝拖曳、收卡）。
  const [peek, setPeek] = useState<{ id: string; at: DOMRect | null } | null>(null)
  const dnd = usePinnedDrag(fullPinned, visiblePinned, names, movePrimary, (id) => setPeek(id ? { id, at: null } : null))
  const onPeek = useCallback((id: string | null, at?: DOMRect) => setPeek(id ? { id, at: at ?? null } : null), [])
  const peekItem = peek ? pinnedItems.find((it) => it.id === peek.id) ?? null : null
  const peekCard =
    peek && peekItem ? (
      <BotStatusCard
        botId={peek.id}
        hints={chipHints(peekItem)}
        anchor={peek.at}
        onClose={() => setPeek(null)}
        onLegend={() => {
          setPeek(null)
          setLegend(true)
        }}
      />
    ) : null
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
        <ScrollRow items={otherItems} label="在跑或剛完成的 bot" selectedBotId={selectedBotId} />
        {peekCard}
        {legend ? <ChipLegend onClose={() => setLegend(false)} /> : null}
      </>
    )
  }
  // 主力一組、其餘一組，各自換行、各自裁切；收合時主力最多三行，其餘那組在主力也在時一行、只有它時兩行（`lib/chipOverflow.ts`）。
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
              /* 裁切高度用量的，不寫死 px：晶片加了星號與徽章就會變高，寫死會把最後一行切一半（541afe7）。 */
              style={!expanded && clipPx[name] > 0 ? { maxHeight: clipPx[name], overflow: 'hidden' } : undefined}
            >
              {group.map((it) => (
                <Chip key={it.id} it={it} dnd={pinned ? dnd : undefined} onPeek={pinned ? onPeek : undefined} />
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
          title={expanded ? '收合' : moreTitle(hiddenItems)}
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded ? '收合' : `+${hidden}`}
        </button>
      ) : null}
      {/* 電腦版：晶片列尾端一顆「?」開顏色說明（2026-10-04 使用者：電腦版你自己想）。 */}
      {peekCard}
      {legend ? <ChipLegend onClose={() => setLegend(false)} /> : null}
    </div>
  )
}

/** 手機的主力區：4 顆等寬一排、往下換行、收合時最多三排，不橫捲（#344）。 */
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
  /** 收合時藏起來的主力（第 12 格變成「+N」）。 */
  hidden?: ChipItem[]
  /** 展開中：最後多一格「收合」。 */
  collapsible?: boolean
  onToggle?: () => void
  /** 收合時最多幾排；下緣把手上下拖改（2026-10-03 使用者）。 */
  rows?: number
  /** 主力總顆數（含藏起來的）：決定最多拖到幾排。 */
  total?: number
  /** 拖動中即時回報（`done=false`），放開時 `done=true` 才記下來。 */
  onRows?: (n: number, done: boolean) => void
  resizable?: boolean
}) {
  const gridRef = useRef<HTMLDivElement | null>(null)
  const drag = useRef<{ y: number; rows: number; rowPx: number } | null>(null)
  if (items.length === 0) return null
  // 只有一排放得下全部時沒東西可調。
  const canResize = resizable && onRows && rows !== undefined && pinGridRowsNeeded(total) > 1
  const rowPx = () => {
    const chip = gridRef.current?.querySelector<HTMLElement>('.unread-chip')
    const gap = gridRef.current ? parseFloat(getComputedStyle(gridRef.current).rowGap) || 0 : 0
    return chip ? chip.offsetHeight + gap : 0
  }
  // 藏起來的裡面有要你回答／未讀，+N 就吃那個顏色，不然看不出被藏的那顆在等你（跟桌機 `+N` 的提示同一個規則）。
  const moreState = hidden.some((h) => h.needsReply) ? ' needs-reply' : hidden.some((h) => h.unread > 0) ? ' unread' : ''
  return (
    <div className={`unread-bar-wrap row${canResize ? ' pin-resizable' : ''}`}>
      <div className="unread-pin-grid" ref={gridRef} role="status" aria-live="polite" aria-label="主力 bot">
        {items.map((it) => (
          <Chip key={it.id} it={it} dnd={dnd} lamp />
        ))}
        {hidden.length > 0 ? (
          <button
            type="button"
            className={`unread-chip pin-more${moreState}`}
            title={moreTitle(hidden)}
            aria-label={`${moreTitle(hidden)}：${hidden.map((h) => h.name).join('、')}`}
            aria-expanded={false}
            onClick={onToggle}
          >
            <span className="unread-chip-name">+{hidden.length}</span>
          </button>
        ) : collapsible ? (
          <button type="button" className="unread-chip pin-more" aria-expanded title={`收合成 ${rows ?? 3} 排`} onClick={onToggle}>
            <span className="unread-chip-name">收合</span>
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
          aria-label="主力區最多幾排（上下拖）"
          aria-valuemin={1}
          aria-valuemax={pinGridRowsNeeded(total)}
          aria-valuenow={Math.min(rows, pinGridRowsNeeded(total))}
          aria-valuetext={`${Math.min(rows, pinGridRowsNeeded(total))} 排`}
          title="上下拖，調整主力區最多幾排"
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
      {PIN_HINT}
    </span>
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
 * 收合時被裁掉的晶片，以及每一組的裁切高度（分行規則見 `lib/chipOverflow.ts`）。永遠全畫、CSS 裁切，不切陣列——
 * render 依賴測量會震盪。裁切高度用量的：寫死 px 在晶片加了星號與徽章變高之後會把最後一行切一半
 * （2026-09-16 使用者截圖）；裁切改變的是那一組自己的高、不影響晶片在組內的位置，所以量得穩。
 * `ordered` 要跟 DOM 裡晶片的順序一致（主力組在前）。
 */
function useOverflowChips(
  barRef: RefObject<HTMLDivElement | null>,
  active: boolean,
  ordered: ChipItem[],
): { hiddenItems: ChipItem[]; clipPx: Record<GroupName, number> } {
  // 一份 state：兩份分開寫的話，同一次量測會寫兩次 state（oxlint `set-state-in-effect`），畫面也可能中間有一幀不一致。
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
    // 值沒變就不寫 state，否則 layout effect 每幀重繪。
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
