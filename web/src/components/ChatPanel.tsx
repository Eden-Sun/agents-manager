import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode, RefObject } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { BotKind, Message } from '../api/types'
import { attachCommandOf, botLamp, composerState, liveReplyOf, projectHostName, useStore } from '../store/store'
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
 */
export function LiveBubble({
  text,
  activity,
  from,
  kind,
}: {
  text: string | null
  activity?: string | null
  from?: ReactNode
  kind?: BotKind
}) {
  const act = activity?.trim() ? activity.trim() : null
  return (
    <article className={`msg assistant live${text ? ' streaming' : ''}`} aria-live="polite">
      <div className="msg-meta msg-meta-above">
        <div className="msg-meta-left">
          {kind ? <span className={`kind-mark ${kind}`} aria-hidden="true" /> : null}
          {from ? <span className="msg-from">{from}</span> : null}
          <span>{text ? '輸出中…' : (act ?? '等待回覆（hook）…')}</span>
        </div>
      </div>
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
  const ref = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  // Follow the tail (new messages, live output growing) only while the user is at the bottom.
  useLayoutEffect(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [messages, working, liveText, liveActivity])

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
      {inFlight || working ? <LiveBubble text={liveText} activity={liveActivity} /> : null}
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
  // v4.0: the draft lives in the store (per bot, mirrored to localStorage) so switching
  // bots / tabs and reloading keep it; it is cleared only on a successful send.
  const draftKey = `bot:${botId}` as const
  const text = useStore((s) => s.drafts[draftKey] ?? '')
  const setDraft = useStore((s) => s.setDraft)
  const setText = (v: string) => setDraft(draftKey, v)
  const [sending, setSending] = useState(false)
  const ref = inputRef

  useEffect(() => {
    if (!state.disabled) ref.current?.focus()
  }, [state.disabled, botId, ref, forceFocus])

  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(200, el.scrollHeight)}px`
  }, [text, ref])

  const submit = () => {
    const body = text.trim()
    // An image on its own is a valid message; text is only required when there is none.
    if ((!body && files.ids.length === 0) || state.disabled || sending || files.uploading) return
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

  return (
    <div className="composer">
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
          disabled={state.disabled || sending}
          placeholder={state.disabled ? state.reason || '目前無法送出訊息' : '輸入訊息…（圖片可直接拖放或貼上）'}
          title="Enter 送出，Shift+Enter 換行；圖片可拖放或貼上"
          onChange={(e) => setText(e.target.value)}
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
          title={files.uploading ? '圖片上傳中…' : undefined}
          onClick={submit}
        >
          {sending ? '送出中…' : files.uploading ? '上傳中…' : '送出'}
        </button>
      </div>
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
            onClick={() => (settingsOpen ? closeSettings() : openSettings(botId))}
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
        <QuotaStrip focusKind={bot.kind} />
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
