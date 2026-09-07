import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { memo, useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode, RefObject } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { BotKind, KindQuota, Message, QuotaWindow, StatusInfo } from '../api/types'
import { effortLabel, quotaKey } from '../api/types'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { cleanLiveActivity, cleanLiveText } from '../store/liveText'
import { typeAlongside } from '../store/alongside'
import { anchorOf, botLamp, composerState, inFlightTurn, liveReplyOf, projectHostName, useStore } from '../store/store'
import { AttachPicker, AttachTray, DropVeil, MessageAttachments, isImageFile, useAttachments, useDropTarget } from './Attachments'
import { BlockedModal } from './BlockedModal'
import { BlockedPanel } from './BlockedPanel'
import { BotSettingsPanel, PersonaMark } from './BotSettingsPanel'
import { BotNameField } from './BotNameField'
import { ConfirmDialog } from './ConfirmDialog'
import { CopyChip } from './CopyChip'
import { HostBadge } from './HostsPanel'
import { GearIcon } from './Icons'
import { useShelfSink } from './ImageShelf'
import { IssuesBar } from './IssuesBar'
import { KindTag } from './KindTag'
import { ModelQuickPicker } from './ModelPicker'
import { MemBadge } from './MemBadge'
import { QuotaStrip } from './QuotaStrip'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { TerminalTab } from './TerminalTab'
import { ToolsHint } from './Tools'

// `hook` 留著只是為了 tooltip 與萬一的 fallback：正常回覆不再標來源（見 `Bubble`）。
const SOURCE_LABEL: Record<string, string> = {
  hook: '回覆',
  terminal_fallback: '終端擷取',
  transcript: '對話紀錄',
  web: '網頁訊息',
  system: '系統通知',
}

export const KIND_TITLE: Record<BotKind, string> = {
  claude: 'Claude',
  codex: 'Codex',
  grok: 'Grok',
}

/**
 * 訊息時間只到分。秒數在對話裡沒有人在讀，但它是每一則訊息旁邊都有的一串數字——
 * 精確到秒的完整時間仍在 `title` 裡。（Team 時間軸是事件記錄，那邊保留秒。）
 */
function timeOf(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit' })
}

/**
 * One message, shown in full (long content scrolls with the list — nothing is folded).
 * Metadata (speaker / recipients + time) always sits ABOVE the bubble (18px row).
 */
/**
 * 一則訊息。`memo`：串流中每個 `turn_progress`（每秒兩三個）都會讓 `MessageList` 重新
 * render，沒有 memo 的話畫面上每一則的 react-markdown 都跟著重新解析——200 則時每個 tick
 * 一兩百毫秒的 long task（issue #8）。訊息物件本身不會變，所以 shallow 比較就夠。
 */
export const Bubble = memo(function Bubble({
  msg,
  from,
  kind,
  flash,
}: {
  msg: Message
  from?: ReactNode
  kind?: BotKind
  /** 被「這回合的提問」浮窗捲過來時閃一下（見 `LastAskPeek`）。 */
  flash?: boolean
}) {
  const fallback = msg.source === 'terminal_fallback'
  const system = msg.role === 'system'
  const rail = system || msg.source === 'hook' || msg.source === 'system'

  return (
    /* `data-msg-id`：唯一能從清單外面（浮窗、之後的搜尋）指回某一則訊息的把手。 */
    <article className={`msg ${msg.role}${rail ? ' rail' : ''}${flash ? ' flash' : ''}`} data-msg-id={msg.id}>
      <div className="msg-meta msg-meta-above">
        <div className="msg-meta-left">
          {kind ? <span className={`kind-mark ${kind}`} aria-hidden="true" /> : null}
          {from ? <span className="msg-from">{from}</span> : null}
          {/* 來源只在「不是正常那條路」時才標。`hook` 是每一則回覆的常態，在每顆氣泡上
              印一次「回覆」等於沒說話；會影響你要不要信這段文字的是另外那幾種——終端
              擷取、對話紀錄、系統通知。 */}
          {system || msg.source === 'system' ? (
            <span className="src-tag mono" title={`訊息來源：${msg.source}`}>
              {SOURCE_LABEL[msg.source] ?? msg.source}
            </span>
          ) : msg.role === 'assistant' && msg.source !== 'hook' ? (
            <span className={`src-tag${fallback ? ' fallback' : ''}`} title={`訊息來源：${msg.source}`}>
              {SOURCE_LABEL[msg.source] ?? msg.source}
            </span>
          ) : null}
          {fallback || msg.incomplete ? <span className="meta-warn">可能不完整</span> : null}
        </div>
        {system ? null : (
          <time className="msg-time" dateTime={msg.created_at} title={msg.created_at}>
            {timeOf(msg.created_at)}
          </time>
        )}
      </div>
      <div className={`bubble${msg.role === 'assistant' && !fallback ? ' md' : ''}${rail ? ' rail' : ''}`}>
        {!msg.content ? (
          <em style={{ opacity: 0.6 }}>（空白訊息）</em>
        ) : msg.role === 'assistant' && !fallback ? (
          <Markdown remarkPlugins={[remarkGfm]}>{msg.content}</Markdown>
        ) : (
          msg.content
        )}
        {msg.attachments.length ? <MessageAttachments items={msg.attachments} /> : null}
        <TerminalSnapshot msg={msg} />
      </div>
    </article>
  )
})

/**
 * The whole pane as it looked when a terminal-fallback message was captured.
 *
 * `content` is a slice cut out of this, and the cut is what goes wrong: the fallback fires
 * 5s after a `working -> idle` edge, so a screen that still says `Running 1 shell command…`
 * gets stored as the reply while the real answer prints a moment later. The daemon has kept
 * the full screen all along (`messages.terminal_snapshot`, already on the wire) — it just was
 * not shown anywhere, so the answer looked lost when it was one click away.
 */
function TerminalSnapshot({ msg }: { msg: Message }) {
  const [open, setOpen] = useState(false)
  const snap = msg.terminal_snapshot?.trim()
  if (!snap) return null
  // Nothing to expand when the cut kept everything there was.
  if (snap === msg.content.trim()) return null
  return (
    <div className="msg-snapshot">
      <button type="button" className="disclosure sub" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span className="chev">{open ? '▼' : '▶'}</span> 完整終端畫面
        <span className="disclosure-note">{snap.length} 字</span>
      </button>
      {open ? <pre className="msg-snapshot-body">{snap}</pre> : null}
    </div>
  )
}

/**
 * The in-flight turn's tail: a live bubble with the partial reply (`turn_progress`, API.md
 * v3.9) once there is text, otherwise the typing indicator. Styled like an assistant bubble
 * so it turns into the final message in place.
 *
 * The meta line is three-state: streaming text → 「輸出中…」; no text but an `activity` row
 * (API.md v4.1, e.g. `Thinking… (12s · ↑ 1.2k tokens)`) → that row verbatim, so a long
 * thinking / tool phase is not silent; neither → 「等待回覆…」. `activity` comes
 * straight off the terminal, so it is rendered as plain text, never Markdown.
 *
 * `alert` (API.md v4.2) is the one thing that outranks all of it: while the CLI is retrying an
 * upstream failure the spinner keeps spinning and the turn stays in flight, so the bubble would
 * otherwise look perfectly healthy. It gets its own warning row under the meta line.
 */
export function LiveBubble({
  text,
  activity,
  alert,
  from,
  kind,
  action,
}: {
  text: string | null
  activity?: string | null
  alert?: string | null
  from?: ReactNode
  kind?: BotKind
  /** 這一泡泡專屬的逃生門（`AbandonTurnAction`）；放在狀態列右端，沒有就不佔位。 */
  action?: ReactNode
}) {
  const act = activity?.trim() ? activity.trim() : null
  const warn = alert?.trim() ? alert.trim() : null
  return (
    <article className={`msg assistant live${text ? ' streaming' : ''}`} aria-live="polite">
      <div className={`bubble${text ? ' md' : ''}`}>
        {text ? (
          <>
            <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>
            <span className="caret" aria-hidden="true" />
          </>
        ) : (
          <TypingDots />
        )}
      </div>
      {/* Below the bubble, unlike a finished message: the status belongs at the growing
          edge of the output, which is where the eye already is. */}
      <div className="msg-meta msg-meta-below">
        <div className="msg-meta-left">
          {kind ? <span className={`kind-mark ${kind}`} aria-hidden="true" /> : null}
          {from ? <span className="msg-from">{from}</span> : null}
          <span>{text ? '輸出中…' : (act ?? '等待回覆…')}</span>
        </div>
        {action ?? null}
      </div>
      {warn ? (
        <p className="live-alert" role="status">
          <span className="live-alert-mark" aria-hidden="true">
            !
          </span>
          {warn}
        </p>
      ) : null}
    </article>
  )
}

/** Shared empty / loading state for the main area (chat, group, terminal, no selection). */
export function EmptyState({
  loading,
  title,
  icon,
  action,
  children,
}: {
  loading?: boolean
  title?: string
  icon?: ReactNode
  action?: ReactNode
  children?: ReactNode
}) {
  return (
    <div className={`msg-empty${loading ? ' loading' : ''}`} role="status">
      {loading ? <TypingDots /> : icon ? <div className="msg-empty-icon">{icon}</div> : null}
      {title ? <h2 className="msg-empty-title">{title}</h2> : null}
      {children ? <p className="msg-empty-body">{children}</p> : null}
      {action ? <div className="msg-empty-action">{action}</div> : null}
    </div>
  )
}

export function TypingDots() {
  return (
    <span className="typing" aria-hidden="true">
      <i />
      <i />
      <i />
    </span>
  )
}

/**
 * "Jump to the newest" for a scrollback list.
 *
 * `stick` (a ref) already decides whether the list should follow new output, but a ref
 * cannot drive rendering — so the same measurement is mirrored into state, and the button
 * only exists while the user has actually scrolled away from the tail.
 */
export function useScrollTail(deps: unknown[]) {
  const ref = useRef<HTMLDivElement>(null)
  const stick = useRef(true)
  const [atBottom, setAtBottom] = useState(true)

  // Follow the tail (new messages, live output growing) only while the user is at the bottom.
  useLayoutEffect(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
    // The list this hook serves decides what "changed" means.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)

  const onScroll = (e: { currentTarget: HTMLDivElement }) => {
    const el = e.currentTarget
    const near = el.scrollHeight - el.scrollTop - el.clientHeight < 80
    stick.current = near
    setAtBottom((prev) => (prev === near ? prev : near))
  }

  const toBottom = () => {
    const el = ref.current
    if (!el) return
    stick.current = true
    // Jump, don't animate: a long scrollback makes `smooth` take seconds, and the point of
    // the button is to get there at once. The list keeps following the tail afterwards.
    el.scrollTop = el.scrollHeight
    setAtBottom(true)
  }

  return { ref, onScroll, atBottom, toBottom }
}

/**
 * 「載入更早的訊息」（issue #25）。
 *
 * store 只留每個對話最近 `MESSAGE_CAP` 則——第一頁本來就可能有 `has_more`，開一整天之後
 * 更早的也會被截掉。這顆按鈕釘在清單最上方，按下去走 `before=` 分頁把上一頁接回去。
 */
export function LoadEarlier({ id, onLoad }: { id: string; onLoad: (id: string) => void }) {
  const show = useStore((s) => Boolean(s.moreMessages[id]))
  const loading = useStore((s) => Boolean(s.loadingMore[id]))
  if (!show) return null
  return (
    <div className="load-earlier">
      <button type="button" className="btn" disabled={loading} onClick={() => onLoad(id)}>
        {loading ? '載入中…' : '載入更早的訊息'}
      </button>
    </div>
  )
}

export function JumpToBottom({ show, onClick }: { show: boolean; onClick: () => void }) {
  if (!show) return null
  return (
    <button type="button" className="jump-bottom" title="捲到最新訊息" aria-label="捲到最新訊息" onClick={onClick}>
      <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
        <path d="M8 2.6v9.2M4.2 8.4L8 12.2l3.8-3.8" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" />
      </svg>
      <span>最新</span>
    </button>
  )
}

/**
 * 「這回合的提問」浮窗：回合跑起來之後，把使用者最後送出的那則問題釘在對話區最上方。
 *
 * 為什麼要有它：回合一長，agent 的輸出會把提問推到捲軸上面幾千 px 之外，而「它到底在做
 * 我交代的哪件事」正是這段等待裡唯一想確認的事。這時候要嘛往回捲（就失去了輸出的尾巴），
 * 要嘛憑記憶——兩個都不好。
 *
 * 為什麼是浮窗而不是一條固定的列：它只在回合進行中存在，若佔掉版面高度，每個回合的開始
 * 與結束都會讓整串訊息上下跳一次。浮在最上方只蓋住捲軸最上緣（那裡通常是舊訊息），
 * 而「↓ 最新」與 composer 都在下方，不受影響。
 *
 * 互動是兩段的：夾成三行 → 點一下展開全文 → 再點一下捲到那則訊息並閃一下。第二段之後
 * 收回夾行狀態——人已經被送到訊息本身了，浮窗不必再佔著三行以上。內容本來就短（沒被夾）
 * 時沒有第一段，一點就直接捲過去。
 */
function LastAskPeek({ msg, turnId, onJump }: { msg: Message; turnId: string; onJump: () => void }) {
  // 關閉與展開都綁在 turn 上：換回合就自動回到「夾三行、沒關過」，不需要 effect 去清。
  const [ui, setUi] = useState({ turn: turnId, closed: false, open: false })
  if (ui.turn !== turnId) setUi({ turn: turnId, closed: false, open: false })
  const bodyRef = useRef<HTMLButtonElement>(null)
  // 只有真的被夾掉才有「展開」這一段；短提問一點就走。夾行狀態下量，展開後沿用上次的值。
  const [clamped, setClamped] = useState(false)
  useLayoutEffect(() => {
    const el = bodyRef.current
    if (!el || ui.open) return
    setClamped(el.scrollHeight - el.clientHeight > 2)
  }, [msg.content, ui.open])

  if (ui.closed) return null

  const body = msg.content.trim() || (msg.attachments.length ? `（${msg.attachments.length} 張圖片）` : '（空白訊息）')
  const jumpable = !clamped || ui.open

  return (
    <div className="lastq-slot">
      <div className={`lastq${ui.open ? ' open' : ''}`} role="region" aria-label="這回合的提問">
        <div className="lastq-head">
          <span className="lastq-tag">這回合的提問</span>
          <time className="lastq-time" dateTime={msg.created_at} title={msg.created_at}>
            {timeOf(msg.created_at)}
          </time>
          {clamped ? <span className="lastq-hint">{ui.open ? '再點一下 · 捲到這則' : '點一下 · 展開全文'}</span> : null}
          <button
            type="button"
            className="lastq-x"
            aria-label="關閉這回合的提問浮窗"
            title="關閉。這一回合不會再出現，下一回合會再顯示"
            onClick={() => setUi((u) => ({ ...u, closed: true }))}
          >
            ×
          </button>
        </div>
        <button
          ref={bodyRef}
          type="button"
          className="lastq-body"
          aria-expanded={ui.open}
          title={jumpable ? '捲到這則訊息' : '展開全文'}
          onClick={() => {
            if (!jumpable) {
              setUi((u) => ({ ...u, open: true }))
              return
            }
            onJump()
            setUi((u) => ({ ...u, open: false }))
          }}
        >
          {body}
        </button>
      </div>
    </div>
  )
}

/**
 * 這個回合跑多久之後，才把「強制中止」露出來（秒）。
 *
 * 刻意不是立刻出現：正常回合開頭本來就會有一段只在想、沒有輸出的時間，那時候按這顆只會
 * 弄壞好好的回合。等一分鐘之後還停在「等待回覆…」，才比較像是收尾判斷失準。
 */
const ABANDON_AFTER_S = 60

function elapsedLabel(sec: number): string {
  if (sec < 90) return `${Math.floor(sec)} 秒`
  const m = Math.floor(sec / 60)
  return m < 60 ? `${m} 分` : `${Math.floor(m / 60)} 小時 ${m % 60} 分`
}

/**
 * 卡住時的逃生門：`POST /api/turns/:id/abandon`，把這個回合標成 failed，讓 composer 解鎖。
 *
 * 跟標題列的「中斷」是兩件事：「中斷」是對 pane 送 esc（要 agent 停手），這裡完全不碰 agent，
 * 只推翻 daemon 這邊「回合還在跑」的認定。所以它不放在標題列——放在 live 泡泡的狀態列右端，
 * 也就是使用者盯著「等待回覆…」出不來時眼睛已經在的地方，而且一分鐘後才出現。
 */
export function AbandonTurnAction({ botId }: { botId: string }) {
  const turnId = useStore((s) => inFlightTurn(s, botId)?.id ?? null)
  const createdAt = useStore((s) => inFlightTurn(s, botId)?.created_at ?? null)
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? '這個 Bot')
  const abandonTurn = useStore((s) => s.abandonTurn)
  // 確認框記的是「替哪個 turn 開的」而不是單純的布林：回合換人 / 結束時對話框自己就關了，
  // 不需要一個只為了 setState 的 effect（也不會誤把確認套到下一個回合上）。
  const [openFor, setOpenFor] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())

  // 只有真的有回合在跑才計時。turnId 換人時 `now` 可能還是舊的，算出來的 elapsed 會偏小 →
  // 按鈕晚幾秒才出現，這個方向是安全的（寧可晚出現，不要對剛開始的回合誘導誤按）。
  useEffect(() => {
    if (!turnId) return
    const t = setInterval(() => setNow(Date.now()), 5000)
    return () => clearInterval(t)
  }, [turnId])

  const startedMs = createdAt ? Date.parse(createdAt) : Number.NaN
  const elapsed = Number.isFinite(startedMs) ? (now - startedMs) / 1000 : 0

  if (!turnId || elapsed < ABANDON_AFTER_S) return null

  return (
    <>
      <button
        type="button"
        className="live-abandon"
        title={`將這個回合標記為失敗並解開輸入框（已進行 ${elapsedLabel(elapsed)}）。Bot 可能仍在執行。`}
        onClick={() => setOpenFor(turnId)}
      >
        強制中止 · 已 {elapsedLabel(elapsed)}
      </button>
      <ConfirmDialog
        open={openFor === turnId}
        title="強制中止這個回合"
        width={420}
        body={
          <>
            <p className="abandon-note">
              把 <strong>{botName}</strong> 目前這個回合標記為<strong>失敗</strong>，立刻解開輸入框。適用於
              Bot 已完成回覆，但畫面仍顯示等待的情況。
            </p>
            <p className="abandon-note">
              這個操作只更新回合紀錄，Bot 可能仍在執行。如需讓 Bot 停下來，請到「終端」分頁送出 Esc。
            </p>
            <p className="abandon-note">
              <strong>不可逆</strong>：之後 agent 真的回話，daemon 已經配不回這個回合，那則回覆不會出現在對話裡。
            </p>
          </>
        }
        confirmLabel="強制中止"
        danger
        onConfirm={() => {
          setOpenFor(null)
          void abandonTurn(botId, turnId)
        }}
        onCancel={() => setOpenFor(null)}
      />
    </>
  )
}

/**
 * debug 用的 run 識別列：herdr 那邊的 pane / agent / workspace / session，加上 daemon 這邊的
 * run id。以前只有 `.main-status` 的 tooltip 藏著 pane_id，要 hover 又不能複製。
 *
 * 收在標題列底下、預設收合：常態使用不需要它，但要 debug 時一鍵就能全部攤開來複製。
 */
function RunDebugBar({ botId }: { botId: string }) {
  const run = useStore((s) => s.runs[botId] ?? null)
  const agentName = useStore((s) => s.bots.find((b) => b.id === botId)?.agent_name ?? null)
  if (!run) return null
  return (
    <div className="run-debug" role="group" aria-label="Run 識別資訊">
      <span className="run-debug-hint">識別</span>
      <CopyChip label="pane" value={run.pane_id ?? ''} title="herdr pane id：herdr pane send / capture 用的就是它" />
      <CopyChip label="agent" value={agentName ?? ''} title="herdr agent 名稱：herdr agent list 裡對應的那個" />
      <CopyChip label="session" value={run.herdr_session ?? ''} title="pane 所屬的 herdr session" />
      <CopyChip label="workspace" value={run.workspace_id ?? ''} title="herdr workspace id" />
      <CopyChip label="run" value={run.id} title="daemon DB 的 run id：turn 與 message 都掛在它底下" />
    </div>
  )
}

/**
 * 這回合要回想的那則提問：優先找**這個回合自己**的 user 訊息，沒有（bot / team 起頭的回合）
 * 才退回整串的最後一則。退回的那則仍然是「使用者最後說的話」，比什麼都不顯示有用。
 */
function lastAskOf(list: Message[], turnId: string): Message | null {
  let newest: Message | null = null
  for (let i = list.length - 1; i >= 0; i--) {
    const m = list[i]
    if (m.role !== 'user') continue
    if (m.turn_id === turnId) return m
    if (!newest) newest = m
  }
  return newest
}

/** 捲過去之後那則訊息閃多久（毫秒）。 */
const FLASH_MS = 1600

function MessageList({ botId }: { botId: string }) {
  const messages = useStore((s) => s.messages[botId])
  const loaded = useStore((s) => Boolean(s.loadedBots[botId]))
  const working = useStore((s) => s.runs[botId]?.agent_status === 'working')
  const inFlight = useStore((s) => composerState(s, botId).inFlightTurnId !== null)
  const turnId = useStore((s) => inFlightTurn(s, botId)?.id ?? null)
  const [flashId, setFlashId] = useState<string | null>(null)
  // 擷取來的即時文字先過濾掉 CLI 自己的狀態列 / 提示行（`cleanLiveText`），濾光了就回 null，
  // 讓氣泡退回顯示活動摘要。
  const liveText = useStore((s) => cleanLiveText(liveReplyOf(s, botId)?.text))
  const liveActivity = useStore((s) => cleanLiveActivity(liveReplyOf(s, botId)?.activity))
  const liveAlert = useStore((s) => liveReplyOf(s, botId)?.alert ?? null)
  const loadEarlier = useStore((s) => s.loadEarlierMessages)
  const tail = useScrollTail([messages, working, liveText, liveActivity, liveAlert])

  useEffect(() => {
    if (!flashId) return
    const t = setTimeout(() => setFlashId(null), FLASH_MS)
    return () => clearTimeout(t)
  }, [flashId])

  const list = messages ?? []
  const lastAsk = turnId ? lastAskOf(list, turnId) : null

  /**
   * 捲到某一則訊息。用 rect 差而不是 `offsetTop`：`.msg-list` 自己沒有 `position`，
   * offsetParent 會落到 `.msg-list-wrap` 上，算出來的值差一個 padding。
   * `scrollIntoView` 也不用——它會連帶捲動外層容器。
   */
  const jumpTo = (id: string) => {
    const box = tail.ref.current
    const el = box?.querySelector(`[data-msg-id="${CSS.escape(id)}"]`)
    if (!box || !(el instanceof HTMLElement)) return
    const top = el.getBoundingClientRect().top - box.getBoundingClientRect().top + box.scrollTop
    // 浮窗自己蓋住最上緣，多留一點空間，免得捲過去正好被它蓋掉。
    const still = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false
    box.scrollTo({ top: Math.max(0, top - 72), behavior: still ? 'auto' : 'smooth' })
    // 先關再開：連按兩次時 class 一直掛著，CSS 動畫不會自己重播（也順便重設熄滅的計時器）。
    setFlashId(null)
    requestAnimationFrame(() => setFlashId(id))
  }

  return (
    <div className="msg-list-wrap">
    <div className="msg-list" ref={tail.ref} onScroll={tail.onScroll}>
      <LoadEarlier id={botId} onLoad={loadEarlier} />
      {list.length === 0 ? (
        <EmptyState
          loading={!loaded}
          title={loaded ? '開始交代第一個任務' : undefined}
          icon={loaded ? '✦' : undefined}
        >
          {loaded ? '在下方輸入框寫下第一則訊息。' : '載入訊息中…'}
        </EmptyState>
      ) : (
        list.map((m) => <Bubble key={m.id} msg={m} flash={m.id === flashId} />)
      )}
      {inFlight || working ? (
        <LiveBubble text={liveText} activity={liveActivity} alert={liveAlert} action={<AbandonTurnAction botId={botId} />} />
      ) : null}
    </div>
    {turnId && lastAsk ? <LastAskPeek key={botId} msg={lastAsk} turnId={turnId} onJump={() => jumpTo(lastAsk.id)} /> : null}
    <JumpToBottom show={!tail.atBottom && list.length > 0} onClick={tail.toBottom} />
    </div>
  )
}

function Composer({
  botId,
  inputRef,
  hideLock,
  forceFocus,
  files,
}: {
  botId: string
  inputRef: RefObject<HTMLTextAreaElement | null>
  /** Parent already shows a stopped / start bar — skip the duplicate lock strip. */
  hideLock?: boolean
  /** Empty chat: focus as soon as the composer is usable. */
  forceFocus?: boolean
  /** Owned by `ChatPanel` so a drop anywhere in the chat area lands here. */
  files: ReturnType<typeof useAttachments>
}) {
  // `composerState` builds a fresh object every call, so it must be compared shallowly —
  // returning it raw from the selector would spin `useSyncExternalStore`.
  const state = useStore(useShallow((s) => composerState(s, botId)))
  const sendPrompt = useStore((s) => s.sendPrompt)
  const abandonTurn = useStore((s) => s.abandonTurn)
  const interruptBot = useStore((s) => s.interruptBot)
  const abortBot = useStore((s) => s.abortBot)
  const aborting = useStore((s) => Boolean(s.busy[`abort:${botId}`]))
  const queueSend = useStore((s) => s.queueSend)
  const cancelQueuedSend = useStore((s) => s.cancelQueuedSend)
  const sendText = useStore((s) => s.sendText)
  const notify = useStore((s) => s.notify)
  const queued = useStore((s) => s.queuedSends[botId] ?? null)
  // v4.0: the draft lives in the store (per bot, mirrored to localStorage) so switching
  // bots / tabs and reloading keep it; it is cleared only on a successful send.
  const draftKey = `bot:${botId}` as const
  const phone = useMediaQuery(PHONE_QUERY)
  const text = useStore((s) => s.drafts[draftKey] ?? '')
  const setDraft = useStore((s) => s.setDraft)
  const setDraftCursor = useStore((s) => s.setDraftCursor)
  const setText = (v: string) => setDraft(draftKey, v)
  const [sending, setSending] = useState(false)
  const ref = inputRef

  // A focused controlled textarea defaults to the beginning after a reload or bot switch.
  // Restore the saved selection after React has put this bot's draft value into the DOM.
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const currentText = useStore.getState().drafts[draftKey] ?? ''
    const saved = useStore.getState().draftCursors[draftKey]
    const max = currentText.length
    const start = Math.max(0, Math.min(max, saved?.start ?? max))
    const end = Math.max(start, Math.min(max, saved?.end ?? start))
    el.focus()
    el.setSelectionRange(start, end)
  }, [state.disabled, draftKey, ref, forceFocus])

  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(200, el.scrollHeight)}px`
  }, [text, ref])

  const submit = () => {
    const body = text.trim()
    // An image on its own is a valid message; text is only required when there is none.
    if (!body && files.ids.length === 0) return
    // 打字沒被鎖，所以 Enter 也可能落在「送不出去」的狀態：說一聲，別默默吃掉。
    if (state.disabled) {
      notify('error', state.reason || '目前無法送出訊息')
      return
    }
    if (sending || files.uploading) return
    // A turn is still running: park the message instead of eating a 409. The store sends it
    // as soon as that turn ends.
    if (state.queued) {
      queueSend(botId, body, files.ids)
      setText('')
      files.clear()
      return
    }
    setSending(true)
    void sendPrompt(botId, body, files.ids).then((ok) => {
      setSending(false)
      if (ok) {
        setText('')
        files.clear()
      }
    })
  }

  /** 排隊中的那一則優先，否則是輸入框裡打到一半的字。 */
  const pending = queued?.text ?? text

  /**
   * 中止目前這一輪，然後立刻把待送的內容送出去。
   *
   * 分成兩步而不是一個 API：`abortBot` 只把 turn 標成失敗並解鎖，agent 那頭可能還在跑，
   * 所以要等它回來、確認鎖開了才送——不然新的 prompt 會撞上還沒清掉的 in-flight turn。
   */
  const abortAndSend = async () => {
    const body = pending.trim()
    const ids = queued ? queued.attachments : files.ids
    if (!body && ids.length === 0) return
    if (queued) cancelQueuedSend(botId)
    setSending(true)
    await abortBot(botId)
    const ok = await sendPrompt(botId, body, ids)
    setSending(false)
    if (ok) {
      setText('')
      files.clear()
    }
  }

  /**
   * 直接把文字打進 pane，不建立新回合。等同你自己在終端裡輸入：CLI 會自己排,
   * 回覆併在目前這一輪。送出後把輸入框清掉，因為字已經出去了。
   */
  const sendAlongside = async () => {
    const body = pending.trim()
    if (!body) return
    if (queued) cancelQueuedSend(botId)
    setSending(true)
    // 整段文字走 `POST /bots/:id/text`，Enter 由 daemon 另外送（見 store/alongside.ts）：
    // 拆成鍵名的舊寫法會把多行內容的 `\n` 當成一顆不存在的鍵，內容送不完整。
    const ok = await typeAlongside({ sendText }, botId, body)
    setSending(false)
    if (ok) setText('')
  }

  /**
   * 這條的舊條件是「輸入框被鎖住」，但回合進行中並不鎖（可以先打、送出排隊），結果
   * **對話跑起來之後反而沒有任何中斷入口**——連本來就寫在這裡的「中斷回覆」都不會出現，
   * 使用者只能去停掉整個 bot。所以 in-flight 也顯示這一條，只是語氣不同（`.running`）。
   */
  const showLock = !hideLock && Boolean(state.reason) && (state.disabled || Boolean(state.inFlightTurnId))

  const nothingToSend = !text.trim() && files.ids.length === 0

  const syncCursor = () => {
    const el = ref.current
    if (el) setDraftCursor(draftKey, el.selectionStart, el.selectionEnd)
  }

  return (
    <div className="composer">
      {queued ? (
        <div className="composer-queued" role="status">
          <span className="composer-queued-label">已排隊，這回合結束後送出：</span>
          <span className="composer-queued-text" title={queued.text}>
            {queued.text || `（${queued.attachments.length} 張圖片）`}
          </span>
          <button
            type="button"
            className="mini-btn"
            title="取消排隊，把訊息放回輸入框"
            onClick={() => {
            cancelQueuedSend(botId)
            setText(queued.text)
            setDraftCursor(draftKey, queued.text.length)
          }}
          >
            取消
          </button>
        </div>
      ) : null}
      {showLock ? (
        <div className={`composer-lock${state.disabled ? '' : ' running'}`} role="status">
          {/* 拿掉 ⛔：emoji 吃不到 `color`（OS 自己上色），跟琥珀色的框對不上，
              每個平台長得也不一樣。框與文字本身已經是訊號。 */}
          <span>{state.reason}</span>
          {state.unknownTurnId ? (
            <button type="button" className="mini-btn" onClick={() => void abandonTurn(botId, state.unknownTurnId!)}>
              放棄該回合
            </button>
          ) : null}
          {state.inFlightTurnId ? (
            <button type="button" className="mini-btn" title="請 Bot 中斷目前回覆，Bot 仍保持啟動" onClick={() => void interruptBot(botId)}>
              中斷回覆
            </button>
          ) : null}
          {/* 這兩顆是「我不想等」的兩種答案，差別在要不要留住目前這一輪的回覆。 */}
          {state.inFlightTurnId && pending.trim() ? (
            <>
              <button
                type="button"
                className="mini-btn"
                disabled={aborting || sending}
                title={`中止目前這一輪，然後立刻送出：${pending.slice(0, 40)}${pending.length > 40 ? '…' : ''}`}
                onClick={() => void abortAndSend()}
              >
                中止並取代
              </button>
              {/* 併行不是「同時跑兩輪」——daemon 一次只認一個 turn（SPEC §2），第二輪的回覆
                  沒有辦法跟 hook 對上。這顆做的是「直接打進 pane」，跟你自己在終端裡插一句話
                  完全一樣：CLI 自己決定何時處理，回覆會併在目前這一輪裡。 */}
              <button
                type="button"
                className="mini-btn"
                disabled={sending}
                title="不建立新回合，直接把文字打進終端（等同你自己在 pane 裡輸入）。回覆會併在目前這一輪，不會單獨成為一則訊息。"
                onClick={() => void sendAlongside()}
              >
                併行送入
              </button>
            </>
          ) : null}
          {/* 「中斷回覆」是請 agent 停；`esc` 送不進去（pane 沒了、herdr 斷、agent 不理）時
              那一回合會一直卡著、輸入框跟著鎖死。這顆反過來：先解鎖，送鍵只是順帶。 */}
          {state.inFlightTurnId || state.unknownTurnId ? (
            <button
              type="button"
              className="mini-btn danger"
              disabled={aborting}
              title="不等 agent 回應，直接把這回合標成失敗並解開輸入框。Bot 仍保持啟動——它那頭可能還在跑。"
              onClick={() => void abortBot(botId)}
            >
              {aborting ? '中止中…' : '強制中止'}
            </button>
          ) : null}
        </div>
      ) : null}
      <AttachTray items={files.items} onRemove={files.remove} disabled={sending} />
      <div className="composer-box">
        <AttachPicker onFiles={files.add} disabled={state.disabled || sending} />
        <textarea
          ref={ref}
          value={text}
          /* 連線斷了也讓人繼續打（草稿本來就會存），只是送不出去。 */
          disabled={sending}
          /* 手機用短版：括號裡那句在 390px 會把輸入框撐成兩行，而且觸控裝置也拖放不了檔案。
             完整說明留在 `title`（桌機 hover 看得到）。 */
          placeholder={
            state.disabled
              ? `${state.reason || '目前無法送出訊息'}${phone ? '' : '——可以先打，恢復後再送'}`
              : state.queued
                ? `這回合還在跑，先打下一則…${phone ? '' : '（送出會排隊）'}`
                : `輸入訊息…${phone ? '' : '（圖片可直接拖放或貼上）'}`
          }
          title="Enter 送出，Shift+Enter 換行；圖片可拖放或貼上"
          onChange={(e) => {
            setText(e.target.value)
            setDraftCursor(draftKey, e.target.selectionStart, e.target.selectionEnd)
          }}
          onSelect={syncCursor}
          onClick={syncCursor}
          onBlur={syncCursor}
          onKeyUp={syncCursor}
          onPaste={(e) => {
            const imgs = Array.from(e.clipboardData?.files ?? []).filter(isImageFile)
            if (imgs.length === 0) return
            // Only swallow the paste when it really carries images, so copied text still lands.
            e.preventDefault()
            files.add(imgs)
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault()
              submit()
            }
          }}
        />
        <button
          type="button"
          className="send-btn"
          disabled={state.disabled || sending || files.uploading || nothingToSend}
          title={files.uploading ? '圖片上傳中…' : state.queued ? '這回合結束後自動送出' : undefined}
          onClick={submit}
        >
          {sending ? '送出中…' : files.uploading ? '上傳中…' : state.queued ? '排隊送出' : '送出'}
        </button>
      </div>
    </div>
  )
}

/** 168800 → `169k`, 1000000 → `1M`. */
function compactTokens(n: number): string {
  if (n >= 1_000_000) {
    const m = n / 1_000_000
    return `${m >= 10 || Number.isInteger(m) ? Math.round(m) : m.toFixed(1)}M`
  }
  if (n >= 1000) return `${Math.round(n / 1000)}k`
  return String(n)
}

/** claude sends these as raw floats (28.000000000000004); one decimal at most. */
function pct(n: number): string {
  const r = Math.round(n * 10) / 10
  return `${Number.isInteger(r) ? r : r.toFixed(1)}%`
}

function SlItem({ k, children, title, className }: { k: string; children: ReactNode; title?: string; className?: string }) {
  return (
    <span className={`sl-item${className ? ' ' + className : ''}`} title={title}>
      <span className="sl-k">{k}</span>
      <span className="sl-v">{children}</span>
    </span>
  )
}

/**
 * The bot's status bar.
 *
 * The pane's own line is written for a terminal's width — the user's script trims the
 * account to five characters and the model to `OP5` to make it fit. The browser has room,
 * so this renders the *original* statusLine fields instead (`run.status`): the whole email,
 * the real model name, and the context window, which the compressed line has no space for.
 * `status_line` (the pane's exact text) stays as the tooltip, and as the fallback for a bot
 * whose payload has not arrived yet. Bots without a statusLine (codex / grok) show nothing.
 */
/**
 * A status bar for the kinds that have no statusLine *hook*.
 *
 * codex renders its own status line inside the TUI (`[tui] status_line` in
 * `~/.codex/config.toml` — model, cwd, 5h, weekly), and grok likewise; neither can hand it
 * to us the way claude's statusLine command does, and reading it back off the pane would
 * only get the terminal-width-truncated version (`~/…`). Every field it shows is already
 * in the store, so build it from there instead — same shape as claude's, no truncation.
 */
function derivedStatus(
  kind: BotKind,
  model: string | null,
  effort: string | null,
  fast: boolean,
  cwd: string | null,
  quota: KindQuota | null,
): StatusInfo | null {
  if (!model && !quota && !cwd) return null
  const win = (w: QuotaWindow | null | undefined) => ({
    pct: typeof w?.used_pct === 'number' ? w.used_pct : null,
    // The quota API gives an ISO string; the bar wants epoch seconds.
    at: w?.resets_at ? Math.floor(new Date(w.resets_at).getTime() / 1000) || null : null,
  })
  const five = win(quota?.five_hour)
  const seven = win(quota?.seven_day)
  return {
    account_email: null,
    account_warning: null,
    model_name: model,
    model_id: null,
    effort,
    thinking: false,
    fast_mode: kind === 'codex' && fast,
    context_used_pct: null,
    context_used_tokens: null,
    context_size: null,
    five_hour_pct: five.pct,
    five_hour_resets_at: five.at,
    seven_day_pct: seven.pct,
    seven_day_resets_at: seven.at,
    cost_usd: null,
    cwd,
    version: null,
    session_name: null,
  }
}

/**
 * `高 · fast · thinking` — the model's *settings*, as opposed to its name. Lives next to the
 * model badge in the header now; the status bar used to carry its own copy of both.
 */
function modelExtraOf(status: StatusInfo | null): string {
  if (!status) return ''
  // 全部用 `·`：`opus-高` 這種連字號黏法讀起來像模型 id 的一部分（見 `ModelTag`）。
  return [status.effort ? effortLabel(status.effort) : null, status.fast_mode ? 'fast' : null, status.thinking ? 'thinking' : null]
    .filter(Boolean)
    .join(' · ')
}

/**
 * One row for the repo chip + the status fields. `hasStatus` is passed rather than inferred:
 * `<StatusLineBar>` is a truthy element even on the render where it returns null, so the row
 * would keep a hairline border for a bot that has no status line at all.
 */
function ContextBar({ issues, status, hasStatus }: { issues: ReactNode; status: ReactNode; hasStatus: boolean }) {
  if (!issues && !hasStatus) return null
  return (
    <div className="context-bar">
      {issues}
      {hasStatus ? status : null}
    </div>
  )
}

function StatusLineBar({ status, text }: { status: StatusInfo | null; text: string | null }) {
  const line = text?.trim() ?? ''
  if (!status) {
    if (!line) return null
    return (
      <div className="statusline-bar" role="status" title={line}>
        <span className="statusline-text mono">{line}</span>
      </div>
    )
  }

  const ctxDetail =
    status.context_used_tokens !== null && status.context_size !== null
      ? `${compactTokens(status.context_used_tokens)}/${compactTokens(status.context_size)}`
      : null
  return (
    <div className="statusline-bar" role="status" title={line || undefined}>
      {status.account_warning ? (
        <SlItem k="帳號" className="sl-account sl-warn" title={status.account_warning}>
          ⚠ 未登入，用的是預設帳號
        </SlItem>
      ) : status.account_email ? (
        // 手機上這一欄最長也最不急（側欄與設定都看得到），`sl-account` 讓 CSS 把它收掉。
        <SlItem k="帳號" className="sl-account">
          {status.account_email}
        </SlItem>
      ) : status.cwd ? (
        <SlItem k="目錄" title={status.cwd}>
          {status.cwd.replace(/^\/Users\/[^/]+/, '~')}
        </SlItem>
      ) : null}
      {status.context_used_pct !== null ? (
        <SlItem k="context" title={ctxDetail ? `已用 ${ctxDetail} tokens` : undefined}>
          {pct(status.context_used_pct)}{ctxDetail ? <span className="sl-dim"> · {ctxDetail}</span> : null}
        </SlItem>
      ) : null}
      {status.cost_usd !== null ? <SlItem k="花費">${status.cost_usd.toFixed(2)}</SlItem> : null}
      {/* 版本是這一列最不常看的一欄，窄視窗第一個讓位（`sl-version`）。 */}
      {status.version ? (
        <SlItem k="版本" className="sl-version">
          {status.version}
        </SlItem>
      ) : null}
    </div>
  )
}

/** blocked 到「自動彈出全畫面終端」之間的緩衝，見下方 armed 的說明。 */
const AUTO_OPEN_DELAY_MS = 1000

export function ChatPanel({ onOpenSidebar }: { onOpenSidebar: () => void }) {
  const botId = useStore((s) => s.selectedBotId)
  const bot = useStore((s) => s.bots.find((b) => b.id === s.selectedBotId) ?? null)
  const run = useStore((s) => (s.selectedBotId ? (s.runs[s.selectedBotId] ?? null) : null))
  const lamp = useStore((s) => (s.selectedBotId ? botLamp(s, s.selectedBotId) : 'offline'))
  const hostName = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null))
  const hostUp = useStore((s) => {
    const name = projectHostName(s, s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null)
    return name === 'local' || (s.hosts.find((h) => h.name === name)?.connected ?? false)
  })
  const tab = useStore((s) => s.rightTab)
  const setRightTab = useStore((s) => s.setRightTab)
  const settingsBotId = useStore((s) => s.settingsBotId)
  const openSettings = useStore((s) => s.openSettings)
  const closeSettings = useStore((s) => s.closeSettings)
  const startBot = useStore((s) => s.startBot)
  const busy = useStore((s) => s.busy)
  const composerRef = useRef<HTMLTextAreaElement>(null)
  // debug 用的 run 識別列（pane / agent / run id）預設收合，不佔常態版面。
  const [runDebugOpen, setRunDebugOpen] = useState(false)
  // claude hands us its statusLine payload; the other kinds render their status line inside
  // their own TUI, so it is rebuilt from what the store already knows.
  const statusInfo = useStore(
    useShallow((s): StatusInfo | null => {
      const b = s.bots.find((x) => x.id === s.selectedBotId)
      if (!b) return null
      const r = s.runs[b.id] ?? null
      if (r?.status) return r.status
      if (b.kind === 'claude') return null
      // 額度按主機分（SPEC §14）：狀態列講的是這隻 bot，就看它那台的列。
      const host = projectHostName(s, b.project_id)
      const key = quotaKey(host, b.identity ? `${b.kind}:${b.identity}` : b.kind)
      const q = s.quota[key] ?? s.quota[quotaKey(host, b.kind)] ?? null
      const path = s.projects.find((p) => p.id === b.project_id)?.path ?? null
      return derivedStatus(b.kind, b.model, b.effort, b.fast, path, q)
    }),
  )
  const modelExtra = modelExtraOf(statusInfo)
  // Images live outside the store: they only matter until the send that carries them.
  // Held here (not in the composer) so a drop anywhere in the chat area is accepted.
  const files = useAttachments(botId)
  const drop = useDropTarget(files.add, !botId)
  // 右側圖片暫存區要知道「現在這個對話」是誰：點暫存縮圖時，圖片就落進這個托盤（也就是
  // 上傳給這隻 bot）。終端分頁時這個托盤不在畫面上，就別接收——圖會像憑空消失。
  useShelfSink(files.add, botId && bot && (tab !== 'terminal' || settingsBotId === botId) ? bot.name : null)

  const messages = useStore((s) => (botId ? s.messages[botId] : undefined))
  const messagesLoaded = useStore((s) => (botId ? Boolean(s.loadedBots[botId]) : false))
  const chatEmpty = messagesLoaded && (messages?.length ?? 0) === 0
  const composerReason = useStore((s) => (botId ? composerState(s, botId).reason : ''))

  /**
   * agent 需要回應時，整個 herdr 畫面自己跳出來——判斷「該按 y 還是 n」要看的是完整的對話框，
   * 不是對話上方那塊 300px 的截角。只彈**現在正在看的**這個 bot：別的 bot 進 blocked 留給側欄
   * 的紅點，不打斷手上的事。
   *
   * 關掉之後不會再自己彈回來（`dismissed`），直到這個 bot 離開 blocked 又再進去一次——那是另一
   * 個問題，值得再問一次。手動「展開全畫面」隨時可以叫回來。
   *
   * 狀態轉換在 render 當下算完（比對上一次的 bot / blocked），不放進 effect：從 effect 裡
   * setState 會多跑一輪 render，而這個彈窗要跟紅燈同一幀出現。
   */
  const blockedNow = run?.agent_status === 'blocked'
  const [blockedUi, setBlockedUi] = useState({ bot: botId ?? '', blocked: false, armed: false, open: false, dismissed: false })
  if (blockedUi.bot !== (botId ?? '') || blockedUi.blocked !== blockedNow) {
    const sameBot = blockedUi.bot === (botId ?? '')
    // 離開 blocked（或換了 bot）就把「關掉過」忘掉，下一次 blocked 才會再自己彈出來。
    const dismissed = sameBot && blockedNow ? blockedUi.dismissed : false
    const armed = blockedNow && !dismissed
    setBlockedUi({ bot: botId ?? '', blocked: blockedNow, armed, open: armed && blockedUi.open, dismissed })
  }
  // 有些 blocked 是 daemon 自己會按掉的（claude 的滿意度問卷 → `tui_prompts`），一秒內就過去了。
  // 等一下再彈，免得為了那種東西閃一個全畫面視窗出來。
  useEffect(() => {
    if (!blockedUi.armed) return
    const t = setTimeout(() => setBlockedUi((u) => (u.armed ? { ...u, armed: false, open: true } : u)), AUTO_OPEN_DELAY_MS)
    return () => clearTimeout(t)
  }, [blockedUi.armed])
  const blockedFull = blockedUi.open

  if (!botId || !bot) {
    return (
      <>
        <div className="main-head">
          <button
            type="button"
            className="btn menu-btn icon-tip"
            onClick={onOpenSidebar}
            aria-label="開啟側邊欄"
            title="開啟側邊欄"
            data-tip="開啟側邊欄"
          >
            ☰
          </button>
          <span className="main-status">未選擇 Bot</span>
          <span className="spacer" />
          <QuotaStrip />
        </div>
        <EmptyState title="尚未選擇 Bot" icon="◎">
          從左側選擇一個 Bot，或先新增 Project 與 Bot。
        </EmptyState>
      </>
    )
  }

  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'
  const blocked = blockedNow
  const settingsOpen = settingsBotId === botId
  const closeBlockedFull = () => setBlockedUi((u) => ({ ...u, armed: false, open: false, dismissed: true }))

  return (
    <>
      <div className="main-head bot-head">
        <button
          type="button"
          className="btn menu-btn icon-tip"
          onClick={onOpenSidebar}
          aria-label="開啟側邊欄"
          title="開啟側邊欄"
          data-tip="開啟側邊欄"
        >
          ☰
        </button>
        <div className="main-title">
          <div className="main-title-row">
            <StatusLamp lamp={lamp} />
            <BotNameField botId={botId} name={bot.name} />
            {/* Ahead of the badges on purpose: `.main-title-row` clips its own tail when the
                header is busy, and the settings button is the one thing in here that is not
                repeated somewhere else — the badges all are. */}
            <button
              type="button"
              className="icon-btn gear icon-tip"
              aria-label={`${bot.name} 的設定`}
              aria-expanded={settingsOpen}
              title={`設定 ${bot.name}（模型、身份、autostart、刪除）`}
              data-tip={`設定 · ${bot.name}`}
              onClick={(e) => (settingsOpen ? closeSettings() : openSettings(botId, anchorOf(e.currentTarget)))}
            >
              <GearIcon />
            </button>
            <PersonaMark persona={bot.persona} />
            <KindTag kind={bot.kind} />
            <HostBadge host={hostName} connected={hostUp} />
          </div>
          {/* `bot.model` is what was *configured* (null = 由 CLI 自己決定); the statusLine
              reports what the CLI actually loaded, so fall back to that rather than
              showing nothing. The settings (`高 · thinking`) ride along as a dim suffix —
              they used to cost the status bar its own 模型 field. Second row under the name
              so the identity row above doesn't have to yield space to it. */}
          {bot.model || statusInfo?.model_name ? (
            <ModelQuickPicker
              botId={botId}
              kind={bot.kind}
              host={hostName}
              className={`model-tag${bot.model ? '' : ' reported'}`}
              title={
                bot.model
                  ? `點一下改模型（${bot.model}）`
                  : `CLI 預設，實際載入 ${statusInfo?.model_name}。點一下改模型`
              }
            >
              {bot.model ?? statusInfo?.model_name}
              {modelExtra ? <span className="model-tag-extra">{modelExtra}</span> : null}
            </ModelQuickPicker>
          ) : null}
        </div>
        <span className="spacer" />
        {/* pane id 而不是狀態文字：狀態看左邊的燈號就好（它自己帶 tooltip），這個位置留給
            debug 時真正要抄的那串。點一下展開整組識別資訊（agent / session / workspace / run）。
            沒有 pane 就什麼都不放——燈號已經說了它沒在跑。
            擺在右邊那一組（額度、記憶體、分頁）而不是名字後面：它跟額度一樣是「這個 run 的
            數字」，跟在名字後面只會在標題列中間留下一段空白。 */}
        {run?.pane_id ? (
          <button
            type="button"
            className={`main-status pane-toggle run-debug-toggle${runDebugOpen ? ' on' : ''}`}
            aria-expanded={runDebugOpen}
            title={`pane ${run.pane_id}（${LAMP_LABEL[lamp]}）· 點一下展開 run 識別資訊：agent、session、workspace、run id`}
            onClick={() => setRunDebugOpen((v) => !v)}
          >
            <span className="pane-id">{run.pane_id}</span>
            <span className="pane-chev" aria-hidden="true">
              {runDebugOpen ? '▴' : '▾'}
            </span>
          </button>
        ) : null}
        <QuotaStrip focusKind={bot.kind} focusIdentity={bot.identity} host={hostName} />
        {/* 遠端才掛：本機的數字固定在左上角，這裡再放一次只是重複。 */}
        <MemBadge host={hostName} onlyRemote />
        <div className="tabs" role="tablist">
          <button type="button" className="tab" role="tab" aria-selected={tab === 'chat' && !settingsOpen} onClick={() => setRightTab('chat')}>
            對話
          </button>
          <button
            type="button"
            className="tab"
            role="tab"
            aria-selected={tab === 'terminal' && !settingsOpen}
            onClick={() => setRightTab('terminal')}
          >
            終端
          </button>
        </div>
      </div>
      {runDebugOpen ? <RunDebugBar botId={botId} /> : null}
      <ToolsHint focusHost={hostName} focusKinds={[bot.kind]} />
      {/* The repo chip and the status bar were a row each; neither fills one, so they share.
          The issues popup must stay outside an `overflow` box, hence the scrolling is on the
          status half only. The chip is chat-only — the terminal tab has no composer to insert into. */}
      <ContextBar
        issues={
          tab === 'chat' && !settingsOpen ? (
            <IssuesBar projectId={bot.project_id} draftKey={`bot:${botId}`} inputRef={composerRef} />
          ) : null
        }
        status={<StatusLineBar status={statusInfo} text={run?.status_line ?? null} />}
        hasStatus={Boolean(statusInfo) || Boolean(run?.status_line?.trim())}
      />


      {blocked && blockedFull ? <BlockedModal key={botId} botId={botId} onClose={closeBlockedFull} /> : null}

      {tab === 'terminal' && !settingsOpen ? (
        active ? (
          <TerminalTab botId={botId} />
        ) : (
          <EmptyState
            title="終端尚未就緒"
            icon="▭"
            action={
              <button
                type="button"
                className="btn primary"
                disabled={Boolean(busy[`start:${botId}`])}
                onClick={() => void startBot(botId)}
              >
                啟動 {bot.name}
              </button>
            }
          >
            Bot 未在執行中，沒有可讀取的終端。請先啟動 Bot。
          </EmptyState>
        )
      ) : (
        <div className={`chat${drop.over ? ' dropping' : ''}`} {...drop.props}>
          {drop.over ? <DropVeil /> : null}
          {blocked ? (
            <BlockedPanel
              botId={botId}
              paused={blockedFull}
              onExpand={() => setBlockedUi((u) => ({ ...u, armed: false, open: true }))}
            />
          ) : null}
          <MessageList botId={botId} />
          {!active ? (
            <div className="bot-stopped-bar" role="status">
              <span>{composerReason || 'Bot 未在執行中，無法送出訊息'}</span>
              <button
                type="button"
                className="mini-btn primary"
                disabled={Boolean(busy[`start:${botId}`])}
                title={`啟動 ${bot.name}`}
                onClick={() => void startBot(botId)}
              >
                啟動
              </button>
            </div>
          ) : null}
          <Composer botId={botId} inputRef={composerRef} hideLock={!active} forceFocus={chatEmpty && active} files={files} />
          {settingsOpen ? <BotSettingsPanel key={botId} botId={botId} /> : null}
        </div>
      )}
    </>
  )
}
