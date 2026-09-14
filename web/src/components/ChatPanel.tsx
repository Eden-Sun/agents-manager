import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { memo, useEffect, useLayoutEffect, useRef, useState, useId } from 'react'
import type { ReactNode, RefObject } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { BotKind, KindQuota, Message, QuotaWindow, StatusInfo } from '../api/types'
import { effortLabel, quotaKey } from '../api/types'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useEnterToSend } from '../hooks/useEnterToSend'
import { useComposerFocus } from '../hooks/useComposerFocus'
import { useScrollTail } from '../hooks/useScrollTail'
import { useTapCopy } from '../hooks/useTapCopy'
import { cleanLiveActivity, cleanLiveText } from '../store/liveText'
import { typeAlongside } from '../store/alongside'
import { anchorOf, botLamp, composerState, inFlightTurn, liveReplyOf, projectHostName, toolsOfHost, useStore } from '../store/store'
import { AttachPicker, AttachTray, DropVeil, MessageAttachments, useAttachments, useDropTarget } from './Attachments'
import { BlockedBadge } from './BlockedBadge'
import { BlockedModal } from './BlockedModal'
import { BlockedPanel } from './BlockedPanel'
import { BotSettingsPanel, PersonaMark } from './BotSettingsPanel'
import { BotNameField } from './BotNameField'
import { BotSwitcher } from './BotSwitcher'
import { ConfirmDialog } from './ConfirmDialog'
import { onTabListKeyDown } from './tabKeys'
import { CopyChip } from './CopyChip'
import { HostBadge } from './HostsPanel'
import { UpdateBadge } from './UpdateBadge'
import { TurnErrorBadge } from './TurnErrorBadge'
import { HostShellPanel } from './HostShellPanel'
import { GearIcon, GitIcon } from './Icons'
import { useShelfSink } from './ImageShelf'
import { IssuesBar } from './IssuesBar'
import { GitBar } from './GitBar'
import { KindIcon, KindTag } from './KindTag'
import { Modal } from './Modal'
import { ModelQuickPicker } from './ModelPicker'
import { RuntimeDriftBadge } from './RuntimeDriftBadge'
import { trimClippedTail } from '../lib/statusLineTail'
import { quotedFrom } from '../lib/agmQuote'
import { relayPreview } from '../lib/relayPreview'
import { runtimeKnown } from '../lib/runtimeDrift'
import { shortModel } from '../lib/shortModel'
import { MemBadge } from './MemBadge'
import { QuotaStrip } from './QuotaStrip'
import { PrimaryStar } from './PrimaryStar'
import { UnreadChip } from './UnreadChip'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { TerminalTab } from './TerminalTab'
import { ToolsHint } from './Tools'
import './chatPanel.css'

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

/** 訊息時間只到分；完整時間在 `title`。 */
function timeOf(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit' })
}

/** Codex 帳號提示是給 TUI 看的（`Run /usage`）；對話裡只說還有幾次重置。 */
function systemNoticeText(content: string): string {
  const m = content.match(/^You have (\d+) usage limit resets? available\b/i)
  if (!m) return content
  const n = m[1]
  return n === '1' ? 'Codex 還有 1 次額度重置可用' : `Codex 還有 ${n} 次額度重置可用`
}

/** `relay_from` 有值＝別的 bot 代為交辦，不標會被當成使用者自己派的工；對不到 bot 就退回 ID。 */
const RelayFrom = memo(function RelayFrom({ fromId, toId }: { fromId: string; toId: string | null }) {
  const names = useStore(
    useShallow((s) => ({
      from: s.bots.find((b) => b.id === fromId)?.name ?? '',
      to: toId ? (s.bots.find((b) => b.id === toId)?.name ?? '') : '',
    })),
  )
  // `daemon` 是哨符不是 bot id（`agent_relay::DAEMON_SENDER`）：排程／daemon 自發，沒有人按過。
  const daemon = fromId === 'daemon'
  return (
    <span
      className={`msg-targets relay${names.from ? '' : ' daemon'}`}
      title={daemon ? 'daemon 自動觸發的訊息（排程／自動化，不是人送的）' : '由其他 agent 代為交辦的訊息（不是你送的）'}
    >
      {daemon ? 'daemon 自動觸發' : names.from || fromId}
      {!daemon && names.to ? ` → ${names.to}` : ''}
    </span>
  )
})

/** 一則訊息。`memo`：串流 tick 會重 render 清單，不 memo 就每則重解析 Markdown（issue #8）。 */
export const Bubble = memo(function Bubble({
  msg,
  from,
  fromClassName,
  fromTitle,
  kind,
  flash,
}: {
  msg: Message
  from?: string
  fromClassName?: string
  fromTitle?: string
  kind?: BotKind
  /** 被「這回合的提問」浮窗捲過來時閃一下（見 `LastAskPeek`）。 */
  flash?: boolean
}) {
  const fallback = msg.source === 'terminal_fallback'
  const system = msg.role === 'system'
  const rail = system || msg.source === 'hook' || msg.source === 'system'
  const daemonNotice = msg.role === 'user' && msg.content.trimStart().startsWith('[AG Man 通知]')
  // 使用者貼進來的 AGM 裁示（沒有 relay_from）另標（2026-09-12 使用者：「算在 AGM 訊息不算在 user」）。
  // 只認開頭 `AGM …：` 或 `[AGM …]`，句中提到不算。
  const quoted = msg.role === 'user' && !msg.relay_from ? quotedFrom(msg.content) : null
  // 打字送出但沒有無損證據（SPEC §4.4a）：留一個看得到的標記讓人核對。
  const unverified = useStore((s) =>
    msg.role === 'user' && msg.turn_id && msg.bot_id ? s.turns[msg.bot_id]?.[msg.turn_id]?.unverified === true : false,
  )
  // 代發與轉述（AGM 交辦、bot 互轉）預設收合成一行（2026-09-14 使用者：「不用全顯示，收合就好」）。
  const preview = msg.role === 'user' && (msg.relay_from || quoted) ? relayPreview(msg.content) : null
  const [expanded, setExpanded] = useState(false)
  const folded = Boolean(preview?.truncated) && !expanded
  const notify = useStore((s) => s.notify)
  const tapCopy = useTapCopy(system ? systemNoticeText(msg.content) : msg.content, (ok) => {
    notify(ok ? 'info' : 'error', ok ? '已複製訊息' : '複製失敗，請長按選取文字複製')
  })

  return (
    /* `data-msg-id`：從清單外（浮窗）指回這則訊息的把手。 */
    <article className={`msg ${msg.role}${rail ? ' rail' : ''}${daemonNotice ? ' daemon-notice' : ''}${flash ? ' flash' : ''}`} data-msg-id={msg.id}>
      <div className="msg-meta msg-meta-above">
        <div className="msg-meta-left">
          {kind ? <span className={`kind-mark ${kind}`} aria-hidden="true" /> : null}
          {from ? (
            <span className={`msg-from${fromClassName ? ` ${fromClassName}` : ''}`} title={fromTitle}>
              {from}
            </span>
          ) : null}
          {msg.role === 'user' && msg.relay_from ? <RelayFrom fromId={msg.relay_from} toId={msg.bot_id ?? null} /> : null}
          {quoted ? (
            <span className="msg-targets relay quoted" title={`這一則的內容來自 ${quoted}，不是使用者自己打的`}>
              {quoted}（轉述）
            </span>
          ) : null}
          {daemonNotice ? <span className="src-tag daemon" title="daemon 自動通知，不是使用者直接輸入">daemon 通知</span> : null}
          {unverified ? (
            <span className="src-tag fallback" title="已打字送出，但這個 bot 沒有可以逐字核對的紀錄（grok、遠端主機、codex 尚未回報 session），請到終端分頁確認它真的收到">
              未驗證送達
            </span>
          ) : null}
          {/* 來源只在非 `hook` 常態時才標（影響可信度的那幾種）。 */}
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
      <div
        className={`bubble bubble-copyable${msg.role === 'assistant' && !fallback ? ' md' : ''}${rail ? ' rail' : ''}${folded ? ' folded' : ''}`}
        /* 不放 title：原生 tooltip 會壓在正文上（2026-09-12 使用者截圖）。 */
        {...tapCopy}
      >
        {!msg.content ? (
          <em style={{ opacity: 0.6 }}>（空白訊息）</em>
        ) : msg.role === 'assistant' && !fallback ? (
          <Markdown remarkPlugins={[remarkGfm]}>{msg.content}</Markdown>
        ) : folded && preview ? (
          preview.text
        ) : (
          system ? systemNoticeText(msg.content) : msg.content
        )}
        {msg.attachments.length && !folded ? <MessageAttachments items={msg.attachments} /> : null}
        <TerminalSnapshot msg={msg} />
      </div>
      {preview?.truncated ? (
        <button type="button" className="disclosure sub relay-fold" aria-expanded={expanded} onClick={() => setExpanded((v) => !v)}>
          <span className="chev">{expanded ? '▼' : '▶'}</span> {expanded ? '收合' : '展開全文'}
          <span className="disclosure-note">
            {preview.length} 字{msg.attachments.length ? `・${msg.attachments.length} 個附件` : ''}
          </span>
        </button>
      ) : null}
    </article>
  )
})

/**
 * Full pane at terminal-fallback capture time: the cut `content` can miss the real answer
 * (fallback fires 5s after working→idle). 只掛 `terminal_fallback`，系統 pill 不放（框中框）。
 */
function TerminalSnapshot({ msg }: { msg: Message }) {
  const [open, setOpen] = useState(false)
  if (msg.source !== 'terminal_fallback') return null
  const snap = msg.terminal_snapshot?.trim()
  if (!snap) return null
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
 * In-flight turn tail (`turn_progress`, API.md v3.9). `activity` (v4.1) is raw terminal text —
 * never Markdown. `alert` (v4.2) gets its own row: a retrying CLI otherwise looks healthy.
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
  from?: string
  kind?: BotKind
  /** 狀態列右端的逃生門（`AbandonTurnAction`）。 */
  action?: ReactNode
}) {
  const act = activity?.trim() ? activity.trim() : null
  const warn = alert?.trim() ? alert.trim() : null
  // 半成品預設收起（2026-09-08）：逐字跳讀了白讀、還會一直推動清單。
  const [open, setOpen] = useState(false)
  const showText = Boolean(text) && open
  return (
    <article className={`msg assistant live${showText ? ' streaming' : ''}`} aria-live="polite">
      <div className={`bubble${showText ? ' md' : ''}`}>
        {showText ? (
          <>
            <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>
            <span className="caret" aria-hidden="true" />
          </>
        ) : (
          <TypingDots />
        )}
      </div>
      {/* Below the bubble: status sits at the growing edge, where the eye is. */}
      <div className="msg-meta msg-meta-below">
        <div className="msg-meta-left">
          {kind ? <span className={`kind-mark ${kind}`} aria-hidden="true" /> : null}
          {from ? <span className="msg-from">{from}</span> : null}
          <span>{text ? '輸出中…' : (act ?? '等待回覆…')}</span>
          {text ? (
            <button type="button" className="live-peek" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
              {open ? '收起內容' : `看目前內容（${text.length} 字）`}
            </button>
          ) : null}
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

/** Owns the turn-progress subscriptions so history lists don't re-render on every tick. */
export function LiveReplyBubble({
  botId,
  kind,
  from,
  abandon = false,
}: {
  botId: string
  kind?: BotKind
  from?: string
  abandon?: boolean
}) {
  const active = useStore((s) => s.runs[botId]?.agent_status === 'working' || composerState(s, botId).inFlightTurnId !== null)
  const text = useStore((s) => cleanLiveText(liveReplyOf(s, botId)?.text))
  const activity = useStore((s) => cleanLiveActivity(liveReplyOf(s, botId)?.activity))
  const alert = useStore((s) => liveReplyOf(s, botId)?.alert ?? null)

  if (!active) return null
  return (
    <LiveBubble
      text={text}
      activity={activity}
      alert={alert}
      kind={kind}
      from={from}
      action={abandon ? <AbandonTurnAction botId={botId} /> : undefined}
    />
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

/** 「載入更早的訊息」（issue #25）：store 只留最近 `MESSAGE_CAP` 則，走 `before=` 分頁接回。 */
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
 * 「這回合的提問」浮窗：輸出把提問推遠時不必往回捲。浮窗不佔高度，免得回合開始／結束讓清單跳動；
 * 貼 composer 下緣（2026-09-08 依使用者要求從上緣移下來，見 UI-DECISIONS）。手機只在回合完成後顯示。
 */
function LastAskPeek({ msg, turnId, onJump }: { msg: Message; turnId: string; onJump: () => void }) {
  // 狀態綁 turn：換回合自動重設，不需要 effect。
  const [ui, setUi] = useState({ turn: turnId, closed: false, open: false })
  if (ui.turn !== turnId) setUi({ turn: turnId, closed: false, open: false })
  const bodyRef = useRef<HTMLButtonElement>(null)
  // 真的被夾掉才有「展開」這一段；只在夾行狀態下量。
  const [clamped, setClamped] = useState(false)
  useLayoutEffect(() => {
    const el = bodyRef.current
    if (!el || ui.open) return
    setClamped(el.scrollHeight - el.clientHeight > 2)
  }, [msg.content, ui.open])

  if (ui.closed) return null

  const body = msg.content.trim() || (msg.attachments.length ? `（${msg.attachments.length} 個附件）` : '（空白訊息）')
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

/** 回合跑多久才露出「強制中止」（秒）：開頭本來就會安靜一陣，太早出現只會誘導誤按。 */
const ABANDON_AFTER_S = 60

function elapsedLabel(sec: number): string {
  if (sec < 90) return `${Math.floor(sec)} 秒`
  const m = Math.floor(sec / 60)
  return m < 60 ? `${m} 分` : `${Math.floor(m / 60)} 小時 ${m % 60} 分`
}

/**
 * 卡住時的逃生門：`POST /api/turns/:id/abandon` 標 failed 解鎖 composer。不碰 agent（不同於送 esc
 * 的「中斷」），只推翻 daemon 的 in-flight 認定。
 */
export function AbandonTurnAction({ botId }: { botId: string }) {
  const turnId = useStore((s) => inFlightTurn(s, botId)?.id ?? null)
  const createdAt = useStore((s) => inFlightTurn(s, botId)?.created_at ?? null)
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? '這個 Bot')
  const abandonTurn = useStore((s) => s.abandonTurn)
  // 記 turn id 而非布林：回合換掉時確認框自動失效，不會套到下一回合。
  const [openFor, setOpenFor] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())

  // 換 turn 時 `now` 可能舊→elapsed 偏小、按鈕晚出現，這方向安全。
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

/** debug 用的 run 識別列（herdr pane/agent/session/workspace＋run id），預設收合、可複製。 */
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

/** 優先這回合自己的 user 訊息；bot 起頭的回合退回整串最後一則。 */
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

/** 訊息是否在可視範圍：提問看得見就不浮浮窗，免得重複（2026-09-11 使用者）。 */
function useMsgVisible(boxRef: RefObject<HTMLDivElement | null>, msgId: string | null, listLen: number): boolean {
  const [visible, setVisible] = useState(false)
  useEffect(() => {
    const box = boxRef.current
    const el = msgId && box ? box.querySelector(`[data-msg-id="${CSS.escape(msgId)}"]`) : null
    if (!box || !(el instanceof HTMLElement)) {
      setVisible(false)
      return
    }
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) setVisible(e.isIntersecting)
      },
      { root: box, threshold: 0 },
    )
    io.observe(el)
    return () => io.disconnect()
    // `listLen`：訊息可能這一輪才掛上來，長度變就重綁。
  }, [boxRef, msgId, listLen])
  return visible
}

function MessageList({ botId }: { botId: string }) {
  const messages = useStore((s) => s.messages[botId])
  const loaded = useStore((s) => Boolean(s.loadedBots[botId]))
  const working = useStore((s) => s.runs[botId]?.agent_status === 'working')
  const inFlight = useStore((s) => composerState(s, botId).inFlightTurnId !== null)
  const inFlightTurnId = useStore((s) => inFlightTurn(s, botId)?.id ?? null)
  const latestCompletedTurnId = useStore((s) => {
    const turns = Object.values(s.turns[botId] ?? {})
      .filter((turn) => turn.status === 'completed' || turn.status === 'completed_fallback')
      .sort((a, b) => a.created_at.localeCompare(b.created_at))
    return turns.at(-1)?.id ?? null
  })
  const phone = useMediaQuery(PHONE_QUERY)
  const [flashId, setFlashId] = useState<string | null>(null)
  const loadEarlier = useStore((s) => s.loadEarlierMessages)
  const tail = useScrollTail([messages, inFlightTurnId])

  useEffect(() => {
    if (!flashId) return
    const t = setTimeout(() => setFlashId(null), FLASH_MS)
    return () => clearTimeout(t)
  }, [flashId])

  const list = messages ?? []
  // 手機只在已完成且有回覆的回合顯示；桌面在執行中顯示。
  const completedAnswered = latestCompletedTurnId !== null && list.some(
    (m) => m.role === 'assistant' && m.turn_id === latestCompletedTurnId,
  )
  const turnId = phone
    ? (!working && !inFlight && completedAnswered ? latestCompletedTurnId : null)
    : inFlightTurnId
  const lastAsk = turnId ? lastAskOf(list, turnId) : null
  const askVisible = useMsgVisible(tail.ref, lastAsk?.id ?? null, list.length)

  // 用 rect 差：`offsetTop` 會差一個 wrap 的 padding；`scrollIntoView` 會連帶捲外層容器。
  const jumpTo = (id: string) => {
    const box = tail.ref.current
    const el = box?.querySelector(`[data-msg-id="${CSS.escape(id)}"]`)
    if (!box || !(el instanceof HTMLElement)) return
    const top = el.getBoundingClientRect().top - box.getBoundingClientRect().top + box.scrollTop
    const still = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false
    box.scrollTo({ top: Math.max(0, top - 72), behavior: still ? 'auto' : 'smooth' })
    // 先關再開：class 一直掛著的話 CSS 動畫不會重播。
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
      <LiveReplyBubble botId={botId} abandon />
    </div>
    {turnId && lastAsk && !askVisible ? (
      <LastAskPeek key={botId} msg={lastAsk} turnId={turnId} onJump={() => jumpTo(lastAsk.id)} />
    ) : null}
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
  /** Parent already shows a stopped bar. */
  hideLock?: boolean
  forceFocus?: boolean
  /** Owned by `ChatPanel` so a drop anywhere in the chat lands here. */
  files: ReturnType<typeof useAttachments>
}) {
  // Fresh object per call: raw from the selector would spin `useSyncExternalStore`.
  const state = useStore(useShallow((s) => composerState(s, botId)))
  const sendPrompt = useStore((s) => s.sendPrompt)
  const abandonTurn = useStore((s) => s.abandonTurn)
  const interruptBot = useStore((s) => s.interruptBot)
  const abortBot = useStore((s) => s.abortBot)
  const aborting = useStore((s) => Boolean(s.busy[`abort:${botId}`]))
  const queueSend = useStore((s) => s.queueSend)
  const cancelQueuedSend = useStore((s) => s.cancelQueuedSend)
  const restoreQueuedSend = useStore((s) => s.restoreQueuedSend)
  const sendText = useStore((s) => s.sendText)
  const notify = useStore((s) => s.notify)
  const queued = useStore((s) => s.queuedSends[botId] ?? null)
  // Draft lives in the store (localStorage) so switching / reload keep it; cleared only on send.
  const draftKey = `bot:${botId}` as const
  const phone = useMediaQuery(PHONE_QUERY)
  const text = useStore((s) => s.drafts[draftKey] ?? '')
  const setDraft = useStore((s) => s.setDraft)
  const setDraftCursor = useStore((s) => s.setDraftCursor)
  const setText = (v: string) => setDraft(draftKey, v)
  const [sending, setSending] = useState(false)
  const ref = inputRef

  // 手機不自動 focus（2026-09-09 使用者：切 bot 就彈鍵盤擋住視線，要打字自己點）。
  useComposerFocus({ draftKey, ref, forceFocus: forceFocus && !phone, autoFocus: !phone })

  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(200, el.scrollHeight)}px`
  }, [text, ref, phone])

  const submit = () => {
    const body = text.trim()
    if (!body && files.ids.length === 0) return
    // 打字沒鎖，Enter 可能送不出：說一聲，別默默吃掉。
    if (state.disabled) {
      notify('error', state.reason || '目前無法送出訊息')
      return
    }
    if (sending || files.uploading) return
    // Turn still running: queue instead of eating a 409.
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
  const enterToSend = useEnterToSend()

  const pending = queued?.text ?? text

  // 中止再送，分兩步：等 `abortBot` 確認解鎖才送，否則新 prompt 會撞上未清的 in-flight turn。
  const abortAndSend = async () => {
    const body = pending.trim()
    const ids = queued ? queued.attachments : files.ids
    if (!body && ids.length === 0) return
    const wasQueued = queued
    if (wasQueued) cancelQueuedSend(botId)
    setSending(true)
    // esc 沒送進終端就別送新的；排隊的放回去。
    const stopped = await abortBot(botId)
    const ok = stopped && (await sendPrompt(botId, body, ids))
    setSending(false)
    if (ok) {
      setText('')
      files.clear()
    } else if (wasQueued) {
      restoreQueuedSend(botId, wasQueued)
    }
  }

  // 直接打進 pane、不建新回合：回覆併在目前這一輪。
  const sendAlongside = async () => {
    const body = pending.trim()
    if (!body) return
    const wasQueued = queued
    if (wasQueued) cancelQueuedSend(botId)
    setSending(true)
    // 整段走 `POST /bots/:id/text`、Enter 另送（見 store/alongside.ts）；拆鍵名會把 `\n` 當鍵弄丟內容。
    const ok = await typeAlongside({ sendText }, botId, body)
    setSending(false)
    if (ok) setText('')
    else if (wasQueued) restoreQueuedSend(botId, wasQueued)
  }

  // in-flight 時輸入框不鎖，但仍要顯示這條，否則回合中沒有中斷入口（`.running`）。
  const showLock = !hideLock && Boolean(state.reason) && (state.disabled || Boolean(state.inFlightTurnId))

  const nothingToSend = !text.trim() && files.ids.length === 0

  const syncCursor = () => {
    const el = ref.current
    if (el) setDraftCursor(draftKey, el.selectionStart, el.selectionEnd)
  }

  return (
    <div className="composer bot-composer">
      {queued ? (
        <div className="composer-queued" role="status">
          <span className="composer-queued-label">已排隊，這回合結束後送出：</span>
          <span className="composer-queued-text" title={queued.text}>
            {queued.text || `（${queued.attachments.length} 個附件）`}
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
          <span title={state.reason}>{phone && !state.disabled && state.inFlightTurnId ? '執行中 · 送出排隊' : state.reason}</span>
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
              {/* 併行＝直接打進 pane，不是第二輪：daemon 一次只認一個 turn（SPEC §2）。 */}
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
          {/* esc 送不進去時回合會卡死；這顆先解鎖，送鍵只是順帶。 */}
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
          rows={phone ? 1 : 2}
          value={text}
          /* 斷線也讓人繼續打（草稿會存）。 */
          disabled={sending}
          /* 手機短版：390px 會撐成兩行，且觸控不能拖放。 */
          placeholder={
            state.disabled
              ? `${state.reason || '目前無法送出訊息'}${phone ? '' : '——可以先打，恢復後再送'}`
              : state.queued
                ? (phone ? '下一則訊息…' : '這回合還在跑，先打下一則…（送出會排隊）')
                : `輸入訊息…${phone ? '' : '（檔案可直接拖放或貼上）'}`
          }
          title="Enter 送出，Shift+Enter 換行；檔案可拖放或貼上"
          onChange={(e) => {
            setText(e.target.value)
            setDraftCursor(draftKey, e.target.selectionStart, e.target.selectionEnd)
          }}
          onSelect={syncCursor}
          onClick={syncCursor}
          onBlur={syncCursor}
          onKeyUp={syncCursor}
          onPaste={(e) => {
            // 貼上帶的檔案一律收（不再只收圖片）；純文字貼上不帶 files，不受影響。
            const pasted = Array.from(e.clipboardData?.files ?? [])
            if (pasted.length === 0) return
            e.preventDefault()
            files.add(pasted)
          }}
          onKeyDown={(e) => {
            if (enterToSend.enterSends && e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault()
              submit()
            }
          }}
          {...enterToSend.props}
        />
        <button
          type="button"
          className="send-btn"
          disabled={state.disabled || sending || files.uploading || nothingToSend}
          title={files.uploading ? '附件上傳中…' : state.queued ? '這回合結束後自動送出' : undefined}
          /* 手機失焦收鍵盤→版面位移→click 不成立；擋 mousedown 保住焦點（2026-09-10）。 */
          onMouseDown={(e) => e.preventDefault()}
          onClick={submit}
        >
          {sending ? '送出中…' : files.uploading ? '上傳中…' : state.queued ? (phone ? '排隊' : '排隊送出') : '送出'}
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
    <span className={`sl-item${className ? ' ' + className : ''}`} title={title} data-k={k}>
      <span className="sl-k">{k}</span>
      <span className="sl-v">{children}</span>
    </span>
  )
}

/** Status bar for kinds without a statusLine hook (codex/grok draw theirs in the TUI): built from the store. */
function derivedStatus(
  kind: BotKind,
  model: string | null,
  effort: string | null,
  fast: boolean,
  cwd: string | null,
  quota: KindQuota | null,
  version: string | null,
): StatusInfo | null {
  if (!model && !quota && !cwd && !version) return null
  const win = (w: QuotaWindow | null | undefined) => ({
    pct: typeof w?.used_pct === 'number' ? w.used_pct : null,
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
    version,
    session_name: null,
  }
}

/** `高 · fast` — the model's settings, shown beside the model badge. */
function modelExtraOf(status: StatusInfo | null): string {
  if (!status) return ''
  // 用 `·` 不用連字號（像 model id）；`thinking` 是常態不放（2026-09-11 使用者：「fable-Low 多一個 thinking 怪怪的」）。
  return [status.effort ? effortLabel(status.effort) : null, status.fast_mode ? 'fast' : null]
    .filter(Boolean)
    .join(' · ')
}

/** Repo chip + status row. `hasStatus` is explicit: `<StatusLineBar>` is truthy even when it renders null. */
function ContextBar({ issues, status, hasStatus, mobileOpen, onClose }: {
  issues: ReactNode
  status: ReactNode
  hasStatus: boolean
  mobileOpen: boolean
  onClose: () => void
}) {
  const phone = useMediaQuery(PHONE_QUERY)
  if (!issues && !hasStatus) return null
  if (phone && !mobileOpen) return null
  const content = (
    <div className="context-bar">
      {issues}
      {hasStatus ? status : null}
    </div>
  )
  return phone ? (
    <Modal open title="Git / 專案資訊" onClose={onClose}>
      {content}
    </Modal>
  ) : content
}

function StatusLineBar({ botId, status, text }: { botId: string; status: StatusInfo | null; text: string | null }) {
  const line = text?.trim() ?? ''
  if (!status) {
    if (!line) return null
    return (
      <div className="statusline-bar" role="status" title={line}>
        {/* CLI 自己截掉的尾巴（`· …`）不畫：那三個點不說任何事。完整原文留在 title。 */}
        <span className="statusline-text mono">{trimClippedTail(line)}</span>
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
        // `sl-account`：手機上 CSS 收掉（最長也最不急）。
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
      {/* 花費不放（2026-09-12 使用者）。版本貼最右，窄視窗先讓位。 */}
      {status.version ? (
        <SlItem k="版本" className="sl-version">
          {status.version}
        </SlItem>
      ) : null}
      <UpdateBadge botId={botId} variant="inline" />
    </div>
  )
}

/** blocked 到自動彈出全畫面終端的緩衝（見 armed）。 */
const AUTO_OPEN_DELAY_MS = 1000

export function ChatPanel({ onOpenSidebar }: { onOpenSidebar: () => void }) {
  const botId = useStore((s) => s.selectedBotId)
  const bot = useStore((s) => s.bots.find((b) => b.id === s.selectedBotId) ?? null)
  const run = useStore((s) => (s.selectedBotId ? (s.runs[s.selectedBotId] ?? null) : null))
  // 這顆 bot 的未讀：看著就清掉，只在人離開時亮。
  const headUnread = useStore((s) => (s.selectedBotId ? (s.botUnread[s.selectedBotId] ?? 0) : 0))
  const phone = useMediaQuery(PHONE_QUERY)
  const lamp = useStore((s) => (s.selectedBotId ? botLamp(s, s.selectedBotId) : 'offline'))
  const hostName = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null))
  const hostUp = useStore((s) => {
    const name = projectHostName(s, s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null)
    return name === 'local' || (s.hosts.find((h) => h.name === name)?.connected ?? false)
  })
  const tab = useStore((s) => s.rightTab)
  const setRightTab = useStore((s) => s.setRightTab)
  const shellView = useStore((s) => s.shellView)
  const closeShellView = useStore((s) => s.closeShellView)
  const panelId = useId()
  const settingsBotId = useStore((s) => s.settingsBotId)
  const openSettings = useStore((s) => s.openSettings)
  const closeSettings = useStore((s) => s.closeSettings)
  const startBot = useStore((s) => s.startBot)
  const busy = useStore((s) => s.busy)
  const composerRef = useRef<HTMLTextAreaElement>(null)
  const [runDebugOpen, setRunDebugOpen] = useState(false)
  const [gitInfoBotId, setGitInfoBotId] = useState<string | null>(null)
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
      const tool = toolsOfHost(s, host)[b.kind]
      const path = s.projects.find((p) => p.id === b.project_id)?.path ?? null
      // SPEC §4.4a：用 run 實際啟動值（`run.runtime_*`），不是下次啟動才生效的 bot 設定。
      const live = runtimeKnown(r)
      return derivedStatus(
        b.kind,
        live ? r!.runtime_model : b.model,
        live ? r!.runtime_effort : b.effort,
        live ? (r!.runtime_fast ?? b.fast) : b.fast,
        path,
        q,
        tool?.version ?? null,
      )
    }),
  )
  const modelExtra = modelExtraOf(statusInfo)
  // Held here (not in the composer) so a drop anywhere in the chat area is accepted.
  const files = useAttachments(botId, botId)
  const drop = useDropTarget(files.add, !botId)
  // 暫存區縮圖落進這個托盤；終端分頁時托盤不在畫面上就不接收，免得圖憑空消失。
  useShelfSink(files.add, botId && bot && (tab !== 'terminal' || settingsBotId === botId) ? bot.name : null)

  const messages = useStore((s) => (botId ? s.messages[botId] : undefined))
  const messagesLoaded = useStore((s) => (botId ? Boolean(s.loadedBots[botId]) : false))
  const chatEmpty = messagesLoaded && (messages?.length ?? 0) === 0
  const composerReason = useStore((s) => (botId ? composerState(s, botId).reason : ''))

  /**
   * 目前看的 bot blocked 時自動彈全畫面終端（要看完整對話框）；關掉後（`dismissed`）要離開 blocked
   * 再進入才重彈。轉換在 render 中算，不放 effect：彈窗要跟紅燈同一幀。
   */
  const blockedNow = run?.agent_status === 'blocked'
  const [blockedUi, setBlockedUi] = useState({ bot: botId ?? '', blocked: false, armed: false, open: false, dismissed: false })
  if (blockedUi.bot !== (botId ?? '') || blockedUi.blocked !== blockedNow) {
    const sameBot = blockedUi.bot === (botId ?? '')
    const dismissed = sameBot && blockedNow ? blockedUi.dismissed : false
    const armed = blockedNow && !dismissed
    setBlockedUi({ bot: botId ?? '', blocked: blockedNow, armed, open: armed && blockedUi.open, dismissed })
  }
  // 延遲再彈：有些 blocked 一秒內會被 daemon 自己按掉（`tui_prompts`），免得閃一下全畫面。
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
            data-tip="開啟側邊欄"
          >
            ☰
          </button>
          <span className="main-status">未選擇 Bot</span>
          <span className="spacer" />
          <QuotaStrip />
        </div>
        <UnreadChip />
        <EmptyState title="尚未選擇 Bot" icon="◎">
          從左側選擇一個 Bot，或先新增 Project 與 Bot。
        </EmptyState>
      </>
    )
  }

  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'
  const blocked = blockedNow
  const settingsOpen = settingsBotId === botId
  /** 主機 shell 當第三個分頁：標題列不變，只有下面的內容換成終端（2026-09-08）。 */
  const shellOpen = shellView !== null && !settingsOpen
  const closeBlockedFull = () => setBlockedUi((u) => ({ ...u, armed: false, open: false, dismissed: true }))
  // 手動點 chip：立刻開，不管關過沒。
  const openBlockedFull = () => setBlockedUi((u) => ({ ...u, armed: false, open: true }))

  return (
    <>
      <div className="main-head bot-head">
        <button
          type="button"
          className="btn menu-btn icon-tip"
          onClick={onOpenSidebar}
          aria-label="開啟側邊欄"
          data-tip="開啟側邊欄"
        >
          ☰
        </button>
        <div className="main-title">
          <div className="main-title-row">
            <StatusLamp lamp={lamp} />
            {/* kind logo 放第二行 model 左邊（2026-09-11 使用者），不佔名字寬度。 */}
            {phone ? <BotSwitcher botId={botId} name={bot.name} /> : <BotNameField botId={botId} name={bot.name} />}
            {/* Ahead of the badges: the row clips its tail, and the gear is the only non-duplicated entry. */}
            <PrimaryStar botId={botId} />
            <button
              type="button"
              className="icon-btn gear icon-tip"
              aria-label={`${bot.name} 的設定`}
              aria-expanded={settingsOpen}
              data-tip={`設定 · ${bot.name}`}
              onClick={(e) => (settingsOpen ? closeSettings() : openSettings(botId, anchorOf(e.currentTarget)))}
            >
              <GearIcon />
            </button>
            <PersonaMark persona={bot.persona} />
            <HostBadge host={hostName} connected={hostUp} />
            <UpdateBadge botId={botId} />
            <RuntimeDriftBadge botId={botId} />
            {headUnread > 0 ? (
              <span className="unread-turns" title={`${headUnread} 個回合已完成，還沒看過`}>
                !{headUnread > 99 ? '99+' : headUnread}
              </span>
            ) : null}
          </div>
          {/* `bot.model` null = CLI 預設，退回 statusLine 回報的實際模型。第二行：標題列已被額度條佔滿（2026-09-10 實測）。 */}
          {phone ? (
            <div className="mobile-bot-version" title={`${bot.kind} · ${statusInfo?.version ?? '版本未回報'}`}>
              <span className={`mobile-kind-icon ${bot.kind}`} role="img" aria-label={bot.kind}><KindIcon kind={bot.kind} /></span>
              {bot.model || statusInfo?.model_name ? (
                <span className="mobile-bot-model">
                  {shortModel(bot.kind, bot.model ?? statusInfo?.model_name ?? null)}
                  {statusInfo?.effort ? ` · ${effortLabel(statusInfo.effort)}` : ''}
                </span>
              ) : null}
              <span className="mobile-bot-ver">{statusInfo?.version?.match(/\d+\.\d+\.\d+(?:[-+][\w.-]+)?/)?.[0] ?? statusInfo?.version ?? '—'}</span>
              <BlockedBadge botId={botId} onOpen={openBlockedFull} />
              <TurnErrorBadge botId={botId} />
            </div>
          ) : <div className="main-title-sub">
            {/* 放第二行：名字列在 1440px＋側欄時放不下 chip，會剪掉 ⚙（實測）；pane id 讓位（blockedBadge.css）。 */}
            <BlockedBadge botId={botId} onOpen={openBlockedFull} />
            {/* 同理：名字列會整顆剪掉，使用者「額度用盡卻沒看到任何提示」（2026-09-12）。 */}
            <TurnErrorBadge botId={botId} />
            <KindTag kind={bot.kind} />
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
                {shortModel(bot.kind, bot.model ?? statusInfo?.model_name ?? null)}
                {modelExtra ? <span className="model-tag-extra">{modelExtra}</span> : null}
              </ModelQuickPicker>
            ) : null}
            {/* pane id（debug 用，點開識別列）；2026-09-11 使用者：從第一排搬到這一排。窄時只剩 `▾`。 */}
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
          </div>}
        </div>
        {phone ? (
          <>
            <button
              type="button"
              className="icon-btn mobile-git-info"
              aria-label="Git / 專案資訊"
              aria-haspopup="dialog"
              aria-expanded={gitInfoBotId === botId}
              onClick={() => setGitInfoBotId(botId)}
            >
              <GitIcon />
            </button>
            {/* 2026-09-11：暫時空著（`:empty` 藏起來）。 */}
            <div className="mobile-primary-row" />
          </>
        ) : null}
        <span className="spacer" />
        <QuotaStrip focusKind={bot.kind} focusIdentity={bot.identity} host={hostName} />
        {/* 遠端才掛：本機的數字固定在左上角，這裡再放一次只是重複。 */}
        <MemBadge host={hostName} onlyRemote />
        {/* UI-DECISIONS〈無障礙語意（#11）〉。 */}
        <div className="tabs" role="tablist" aria-label="主面板" onKeyDown={onTabListKeyDown}>
          <button
            type="button"
            className="tab"
            role="tab"
            id={`${panelId}-tab-chat`}
            aria-controls={`${panelId}-panel`}
            aria-selected={tab === 'chat' && !settingsOpen && !shellOpen}
            onClick={() => {
              closeShellView()
              setRightTab('chat')
            }}
          >
            對話
          </button>
          <button
            type="button"
            className="tab"
            role="tab"
            id={`${panelId}-tab-terminal`}
            aria-controls={`${panelId}-panel`}
            aria-selected={tab === 'terminal' && !settingsOpen && !shellOpen}
            onClick={() => {
              closeShellView()
              setRightTab('terminal')
            }}
          >
            終端
          </button>
          {shellOpen ? (
            <button
              type="button"
              className="tab"
              role="tab"
              id={`${panelId}-tab-shell`}
              aria-controls={`${panelId}-panel`}
              aria-selected
              title="主機 shell（按「關閉」回到對話）"
            >
              shell
            </button>
          ) : null}
        </div>
      </div>
      {/* 識別列緊貼它的開關所在的標題列，排在主力／晶片列之上（2026-09-14 使用者）：展開的東西要出現在
          按下去的地方旁邊，不是隔一整排晶片。 */}
      {runDebugOpen ? <RunDebugBar botId={botId} /> : null}
      <UnreadChip />
      <ToolsHint focusHost={hostName} focusKinds={[bot.kind]} />
      {/* Issues popup must stay outside an `overflow` box; chip is chat-only (no composer in terminal). */}
      <ContextBar
        mobileOpen={gitInfoBotId === botId}
        onClose={() => setGitInfoBotId(null)}
        issues={
          phone || (tab === 'chat' && !settingsOpen) ? (
            <>
              <IssuesBar projectId={bot.project_id} draftKey={`bot:${botId}`} inputRef={composerRef} />
              <GitBar projectId={bot.project_id} />
            </>
          ) : null
        }
        status={<StatusLineBar botId={botId} status={statusInfo} text={run?.status_line ?? null} />}
        hasStatus={Boolean(statusInfo) || Boolean(run?.status_line?.trim())}
      />


      {blocked && blockedFull ? <BlockedModal key={botId} botId={botId} onClose={closeBlockedFull} /> : null}

      {/* `.tab-panel` 是 display: contents，不改排版。 */}
      <div
        className="tab-panel"
        role="tabpanel"
        id={`${panelId}-panel`}
        aria-labelledby={`${panelId}-tab-${shellOpen ? 'shell' : tab === 'terminal' && !settingsOpen ? 'terminal' : 'chat'}`}
      >
      {shellOpen && shellView ? (
        <HostShellPanel key={`${shellView.host}:${shellView.paneId}`} host={shellView.host} paneId={shellView.paneId} cwd={shellView.cwd} embedded />
      ) : tab === 'terminal' && !settingsOpen ? (
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
      </div>
    </>
  )
}
