import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode, RefObject } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { BotKind, KindQuota, Message, QuotaWindow, StatusInfo } from '../api/types'
import { anchorOf, attachCommandOf, botLamp, composerState, liveReplyOf, projectHostName, useStore } from '../store/store'
import { AttachButton } from './AttachButton'
import { AttachPicker, AttachTray, DropVeil, MessageAttachments, isImageFile, useAttachments, useDropTarget } from './Attachments'
import { BlockedPanel } from './BlockedPanel'
import { BotSettingsPanel, PersonaMark } from './BotSettingsPanel'
import { ConfirmDialog } from './ConfirmDialog'
import { HostBadge } from './HostsPanel'
import { GearIcon } from './Icons'
import { IssuesBar } from './IssuesBar'
import { KindTag } from './KindTag'
import { QuotaStrip } from './QuotaStrip'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { TerminalTab } from './TerminalTab'
import { ToolsHint, ToolsHintIcon } from './Tools'

const SOURCE_LABEL: Record<string, string> = {
  hook: 'hook',
  terminal_fallback: 'terminal_fallback',
  transcript: 'transcript',
  web: 'web',
  system: 'system',
}

export const KIND_TITLE: Record<BotKind, string> = {
  claude: 'Claude',
  codex: 'Codex',
  grok: 'Grok',
}

function timeOf(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleTimeString([], { hour12: false })
}

/**
 * One message, shown in full (long content scrolls with the list — nothing is folded).
 * Metadata (speaker / recipients + time) always sits ABOVE the bubble (18px row).
 */
export function Bubble({
  msg,
  from,
  kind,
}: {
  msg: Message
  from?: ReactNode
  kind?: BotKind
}) {
  const fallback = msg.source === 'terminal_fallback'
  const system = msg.role === 'system'
  const rail = system || msg.source === 'hook' || msg.source === 'system'

  return (
    <article className={`msg ${msg.role}${rail ? ' rail' : ''}`}>
      <div className="msg-meta msg-meta-above">
        <div className="msg-meta-left">
          {kind ? <span className={`kind-mark ${kind}`} aria-hidden="true" /> : null}
          {from ? <span className="msg-from">{from}</span> : null}
          {system || msg.source === 'hook' || msg.source === 'system' ? (
            <span className="src-tag mono" title={`messages.source = ${msg.source}`}>
              {SOURCE_LABEL[msg.source] ?? msg.source}
            </span>
          ) : msg.role === 'assistant' ? (
            <span className={`src-tag${fallback ? ' fallback' : ''}`} title={`messages.source = ${msg.source}`}>
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
      </div>
    </article>
  )
}

/**
 * The in-flight turn's tail: a live bubble with the partial reply (`turn_progress`, API.md
 * v3.9) once there is text, otherwise the typing indicator. Styled like an assistant bubble
 * so it turns into the final message in place.
 *
 * The meta line is three-state: streaming text → 「輸出中…」; no text but an `activity` row
 * (API.md v4.1, e.g. `Thinking… (12s · ↑ 1.2k tokens)`) → that row verbatim, so a long
 * thinking / tool phase is not silent; neither → 「等待回覆（hook）…」. `activity` comes
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
}: {
  text: string | null
  activity?: string | null
  alert?: string | null
  from?: ReactNode
  kind?: BotKind
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
          <span>{text ? '輸出中…' : (act ?? '等待回覆（hook）…')}</span>
        </div>
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

function MessageList({ botId }: { botId: string }) {
  const messages = useStore((s) => s.messages[botId])
  const loaded = useStore((s) => Boolean(s.loadedBots[botId]))
  const working = useStore((s) => s.runs[botId]?.agent_status === 'working')
  const inFlight = useStore((s) => composerState(s, botId).inFlightTurnId !== null)
  const liveText = useStore((s) => liveReplyOf(s, botId)?.text ?? null)
  const liveActivity = useStore((s) => liveReplyOf(s, botId)?.activity ?? null)
  const liveAlert = useStore((s) => liveReplyOf(s, botId)?.alert ?? null)
  const ref = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  // Follow the tail (new messages, live output growing) only while the user is at the bottom.
  useLayoutEffect(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [messages, working, liveText, liveActivity, liveAlert])

  const list = messages ?? []

  return (
    <div
      className="msg-list"
      ref={ref}
      onScroll={(e) => {
        const el = e.currentTarget
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80
      }}
    >
      {list.length === 0 ? (
        <EmptyState
          loading={!loaded}
          title={loaded ? '開始交代第一個任務' : undefined}
          icon={loaded ? '✦' : undefined}
        >
          {loaded ? '在下方輸入框寫下第一則訊息。' : '載入訊息中…'}
        </EmptyState>
      ) : (
        list.map((m) => <Bubble key={m.id} msg={m} />)
      )}
      {inFlight || working ? <LiveBubble text={liveText} activity={liveActivity} alert={liveAlert} /> : null}
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
  const queueSend = useStore((s) => s.queueSend)
  const cancelQueuedSend = useStore((s) => s.cancelQueuedSend)
  const notify = useStore((s) => s.notify)
  const queued = useStore((s) => s.queuedSends[botId] ?? null)
  // v4.0: the draft lives in the store (per bot, mirrored to localStorage) so switching
  // bots / tabs and reloading keep it; it is cleared only on a successful send.
  const draftKey = `bot:${botId}` as const
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

  const showLock = !hideLock && state.disabled && Boolean(state.reason)

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
        <div className="composer-lock" role="status">
          <span>⛔ {state.reason}</span>
          {state.unknownTurnId ? (
            <button type="button" className="mini-btn" onClick={() => void abandonTurn(botId, state.unknownTurnId!)}>
              放棄該回合
            </button>
          ) : null}
          {state.inFlightTurnId ? (
            <button type="button" className="mini-btn" onClick={() => void interruptBot(botId)}>
              中斷（esc）
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
          placeholder={
            state.disabled
              ? `${state.reason || '目前無法送出訊息'}——可以先打，恢復後再送`
              : state.queued
                ? '這回合還在跑，先打下一則…（送出會排隊）'
                : '輸入訊息…（圖片可直接拖放或貼上）'
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

/** epoch seconds → `3h25m` / `12m` / `5d4h`, or '' once it is in the past. */
function untilShort(epochSeconds: number): string {
  const ms = epochSeconds * 1000 - Date.now()
  if (!Number.isFinite(ms) || ms <= 0) return ''
  const mins = Math.floor(ms / 60000)
  const d = Math.floor(mins / 1440)
  const h = Math.floor((mins % 1440) / 60)
  const m = mins % 60
  if (d > 0) return `${d}d${h}h`
  if (h > 0) return `${h}h${m}m`
  return `${m}m`
}

function SlItem({ k, children, title }: { k: string; children: ReactNode; title?: string }) {
  return (
    <span className="sl-item" title={title}>
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
  const five = status.five_hour_resets_at ? untilShort(status.five_hour_resets_at) : ''
  const seven = status.seven_day_resets_at ? untilShort(status.seven_day_resets_at) : ''
  const modelExtra = [status.effort, status.fast_mode ? 'fast' : null, status.thinking ? 'thinking' : null]
    .filter(Boolean)
    .join(' · ')

  return (
    <div className="statusline-bar" role="status" title={line || undefined}>
      {status.account_email ? (
        <SlItem k="帳號">{status.account_email}</SlItem>
      ) : status.cwd ? (
        <SlItem k="目錄" title={status.cwd}>
          {status.cwd.replace(/^\/Users\/[^/]+/, '~')}
        </SlItem>
      ) : null}
      {status.model_name ? (
        <SlItem k="模型" title={status.model_id ?? undefined}>
          {status.model_name}
          {modelExtra ? <span className="sl-dim"> · {modelExtra}</span> : null}
        </SlItem>
      ) : null}
      {status.context_used_pct !== null ? (
        <SlItem k="context" title={ctxDetail ? `已用 ${ctxDetail} tokens` : undefined}>
          {pct(status.context_used_pct)}{ctxDetail ? <span className="sl-dim"> · {ctxDetail}</span> : null}
        </SlItem>
      ) : null}
      {status.five_hour_pct !== null ? (
        <SlItem k="5h">
          {pct(status.five_hour_pct)}{five ? <span className="sl-dim"> · 剩 {five}</span> : null}
        </SlItem>
      ) : null}
      {status.seven_day_pct !== null ? (
        <SlItem k="7d">
          {pct(status.seven_day_pct)}{seven ? <span className="sl-dim"> · 剩 {seven}</span> : null}
        </SlItem>
      ) : null}
      {status.cost_usd !== null ? <SlItem k="花費">${status.cost_usd.toFixed(2)}</SlItem> : null}
      {status.version ? <SlItem k="版本">{status.version}</SlItem> : null}
    </div>
  )
}

export function ChatPanel({ onOpenSidebar }: { onOpenSidebar: () => void }) {
  const botId = useStore((s) => s.selectedBotId)
  const bot = useStore((s) => s.bots.find((b) => b.id === s.selectedBotId) ?? null)
  const project = useStore((s) => s.projects.find((p) => p.id === s.bots.find((b) => b.id === s.selectedBotId)?.project_id) ?? null)
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
  const stopBot = useStore((s) => s.stopBot)
  const interruptBot = useStore((s) => s.interruptBot)
  const busy = useStore((s) => s.busy)
  const attachCommand = useStore((s) => attachCommandOf(s, s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null))
  const composerRef = useRef<HTMLTextAreaElement>(null)
  const [stopConfirmOpen, setStopConfirmOpen] = useState(false)
  // claude hands us its statusLine payload; the other kinds render their status line inside
  // their own TUI, so it is rebuilt from what the store already knows.
  const statusInfo = useStore(
    useShallow((s): StatusInfo | null => {
      const b = s.bots.find((x) => x.id === s.selectedBotId)
      if (!b) return null
      const r = s.runs[b.id] ?? null
      if (r?.status) return r.status
      if (b.kind === 'claude') return null
      const key = b.identity ? `${b.kind}:${b.identity}` : b.kind
      const q = s.quota[key] ?? s.quota[b.kind] ?? null
      const path = s.projects.find((p) => p.id === b.project_id)?.path ?? null
      return derivedStatus(b.kind, b.model, b.effort, b.fast, path, q)
    }),
  )
  // Images live outside the store: they only matter until the send that carries them.
  // Held here (not in the composer) so a drop anywhere in the chat area is accepted.
  const files = useAttachments(botId)
  const drop = useDropTarget(files.add, !botId)

  const messages = useStore((s) => (botId ? s.messages[botId] : undefined))
  const messagesLoaded = useStore((s) => (botId ? Boolean(s.loadedBots[botId]) : false))
  const chatEmpty = messagesLoaded && (messages?.length ?? 0) === 0
  const composerReason = useStore((s) => (botId ? composerState(s, botId).reason : ''))

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
          <ToolsHintIcon />
          <QuotaStrip />
          <AttachButton command={attachCommand} />
        </div>
        <EmptyState title="尚未選擇 Bot" icon="◎">
          從左側選擇一個 Bot，或先新增 Project 與 Bot。
        </EmptyState>
      </>
    )
  }

  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'
  const blocked = run?.agent_status === 'blocked'
  const settingsOpen = settingsBotId === botId

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
        <div className="main-title">
          <StatusLamp lamp={lamp} />
          <strong>{bot.name}</strong>
          <PersonaMark persona={bot.persona} />
          <KindTag kind={bot.kind} />
          {bot.model ? (
            <span className="model-tag" title={`模型：${bot.model}`}>
              {bot.model}
            </span>
          ) : null}
          <HostBadge host={hostName} connected={hostUp} />
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
        </div>
        <span
          className={`main-status ${lamp}`}
          title={run ? `run ${run.id}${run.pane_id ? ` ・ pane ${run.pane_id}` : ''}` : undefined}
        >
          {LAMP_LABEL[lamp]}
        </span>
        <span className="spacer" />
        <ToolsHintIcon />
        <QuotaStrip focusKind={bot.kind} focusIdentity={bot.identity} />
        <AttachButton command={attachCommand} compact />
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
        <div className="head-actions">
          <button
            type="button"
            className="mini-btn interrupt-btn"
            disabled={!active}
            title={`中斷 ${bot.name} 目前回合（esc）`}
            onClick={() => void interruptBot(botId)}
          >
            中斷
          </button>
          {active ? (
            <button
              type="button"
              className="mini-btn danger stop-btn"
              disabled={Boolean(busy[`stop:${botId}`])}
              title={`停止 ${bot.name}`}
              onClick={() => setStopConfirmOpen(true)}
            >
              停止
            </button>
          ) : (
            <button
              type="button"
              className="mini-btn primary"
              disabled={Boolean(busy[`start:${botId}`])}
              title={`啟動 ${bot.name}`}
              onClick={() => void startBot(botId)}
            >
              啟動
            </button>
          )}
        </div>
      </div>
      <ToolsHint focusHost={hostName} focusKinds={[bot.kind]} />
      <StatusLineBar status={statusInfo} text={run?.status_line ?? null} />

      <ConfirmDialog
        open={stopConfirmOpen}
        title="停止 Bot"
        body={
          <>
            確定停止 <strong>{bot.name}</strong>
            {project ? (
              <>
                （專案 <strong>{project.label}</strong>）
              </>
            ) : null}
            ？會對 pane 送出 ctrl+c，必要時關閉終端。
          </>
        }
        confirmLabel="停止"
        danger
        width={360}
        onCancel={() => setStopConfirmOpen(false)}
        onConfirm={() => {
          setStopConfirmOpen(false)
          void stopBot(botId)
        }}
      />

      {tab === 'terminal' && !settingsOpen ? (
        active ? (
          <TerminalTab botId={botId} />
        ) : (
          <EmptyState title="終端尚未就緒" icon="▭">
            Bot 未在執行中，沒有可讀取的終端。請先啟動 Bot。
          </EmptyState>
        )
      ) : (
        <div className={`chat${drop.over ? ' dropping' : ''}`} {...drop.props}>
          {drop.over ? <DropVeil /> : null}
          <IssuesBar projectId={bot.project_id} draftKey={`bot:${botId}`} inputRef={composerRef} />
          {blocked ? <BlockedPanel botId={botId} /> : null}
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
