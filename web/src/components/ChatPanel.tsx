import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Message } from '../api/types'
import { botLamp, composerState, liveReplyOf, projectHostName, useStore } from '../store/store'
import { BlockedPanel } from './BlockedPanel'
import { BotSettingsPanel } from './BotSettingsPanel'
import { HostBadge } from './HostsPanel'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { TerminalTab } from './TerminalTab'

const SOURCE_LABEL: Record<string, string> = {
  hook: 'hook',
  terminal_fallback: 'terminal_fallback',
  transcript: 'transcript',
  web: 'web',
  system: 'system',
}

function timeOf(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleTimeString([], { hour12: false })
}

/**
 * One message, shown in full (long content scrolls with the list — nothing is folded).
 * `from` (SPEC §13 group view) sits on the meta line under the bubble — the bot badge on
 * a reply, or the `→ @a, @b` recipient list on a folded user message — so a short reply
 * stays two lines tall. System notes have no meta line; theirs goes above.
 */
export function Bubble({ msg, from }: { msg: Message; from?: ReactNode }) {
  const fallback = msg.source === 'terminal_fallback'
  const system = msg.role === 'system'

  return (
    <article className={`msg ${msg.role}`}>
      {from && system ? <div className="msg-from">{from}</div> : null}
      <div className={`bubble${msg.role === 'assistant' && !fallback ? ' md' : ''}`}>
        {!msg.content ? (
          <em style={{ opacity: 0.6 }}>（空白訊息）</em>
        ) : msg.role === 'assistant' && !fallback ? (
          <Markdown remarkPlugins={[remarkGfm]}>{msg.content}</Markdown>
        ) : (
          msg.content
        )}
      </div>
      {system ? null : (
        <div className="msg-meta">
          {from && msg.role === 'assistant' ? <span className="msg-from">{from}</span> : null}
          <time dateTime={msg.created_at} title={msg.created_at}>
            {timeOf(msg.created_at)}
          </time>
          {msg.role === 'assistant' ? (
            <span className={`src-tag${fallback ? ' fallback' : ''}`} title={`messages.source = ${msg.source}`}>
              {SOURCE_LABEL[msg.source] ?? msg.source}
            </span>
          ) : null}
          {fallback || msg.incomplete ? <span className="meta-warn">可能不完整</span> : null}
          {from && msg.role === 'user' ? <span className="msg-from">{from}</span> : null}
        </div>
      )}
    </article>
  )
}

/**
 * The in-flight turn's tail: a live bubble with the partial reply (`turn_progress`, API.md
 * v3.9) once there is text, otherwise the typing indicator. Styled like an assistant bubble
 * so it turns into the final message in place.
 */
export function LiveBubble({ text, from }: { text: string | null; from?: ReactNode }) {
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
      <div className="msg-meta">
        {from ? <span className="msg-from">{from}</span> : null}
        <span>{text ? '輸出中…' : '等待回覆（hook）…'}</span>
      </div>
    </article>
  )
}

/** Shared empty / loading state for the main area (chat, group, terminal, no selection). */
export function EmptyState({ loading, children }: { loading?: boolean; children: ReactNode }) {
  return (
    <p className={`msg-empty${loading ? ' loading' : ''}`}>
      {loading ? <TypingDots /> : null}
      {children}
    </p>
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
  const ref = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  // Follow the tail (new messages, live output growing) only while the user is at the bottom.
  useLayoutEffect(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [messages, working, liveText])

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
        <EmptyState loading={!loaded}>
          {loaded ? '還沒有訊息。啟動 Bot 之後，在下方輸入框送出第一則訊息。' : '載入訊息中…'}
        </EmptyState>
      ) : (
        list.map((m) => <Bubble key={m.id} msg={m} />)
      )}
      {inFlight || working ? <LiveBubble text={liveText} /> : null}
    </div>
  )
}

function Composer({ botId }: { botId: string }) {
  // `composerState` builds a fresh object every call, so it must be compared shallowly —
  // returning it raw from the selector would spin `useSyncExternalStore`.
  const state = useStore(useShallow((s) => composerState(s, botId)))
  const sendPrompt = useStore((s) => s.sendPrompt)
  const abandonTurn = useStore((s) => s.abandonTurn)
  const interruptBot = useStore((s) => s.interruptBot)
  const [text, setText] = useState('')
  const [sending, setSending] = useState(false)
  const ref = useRef<HTMLTextAreaElement>(null)

  useEffect(() => {
    if (!state.disabled) ref.current?.focus()
  }, [state.disabled, botId])

  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(200, el.scrollHeight)}px`
  }, [text])

  const submit = () => {
    const body = text.trim()
    if (!body || state.disabled || sending) return
    setSending(true)
    void sendPrompt(botId, body).then((ok) => {
      setSending(false)
      if (ok) setText('')
    })
  }

  return (
    <div className="composer">
      {state.disabled && state.reason ? (
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
      <div className="composer-box">
        <textarea
          ref={ref}
          value={text}
          disabled={state.disabled || sending}
          placeholder={state.disabled ? '目前無法送出訊息' : '輸入訊息…'}
          title="Enter 送出，Shift+Enter 換行"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault()
              submit()
            }
          }}
        />
        <button type="button" className="send-btn" disabled={state.disabled || sending || !text.trim()} onClick={submit}>
          {sending ? '送出中…' : '送出'}
        </button>
      </div>
    </div>
  )
}

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
  const stopBot = useStore((s) => s.stopBot)
  const interruptBot = useStore((s) => s.interruptBot)
  const busy = useStore((s) => s.busy)

  if (!botId || !bot) {
    return (
      <>
        <div className="main-head">
          <button type="button" className="btn menu-btn" onClick={onOpenSidebar}>
            ☰
          </button>
          <span className="main-status">未選擇 Bot</span>
        </div>
        <EmptyState>從左側選擇一個 Bot，或先新增 Project 與 Bot。</EmptyState>
      </>
    )
  }

  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'
  const blocked = run?.agent_status === 'blocked'
  const settingsOpen = settingsBotId === botId

  return (
    <>
      <div className="main-head">
        <button type="button" className="btn menu-btn" onClick={onOpenSidebar} aria-label="開啟側邊欄">
          ☰
        </button>
        <div className="main-title">
          <StatusLamp lamp={lamp} />
          <strong>{bot.name}</strong>
          <span className={`kind-tag ${bot.kind}`}>{bot.kind}</span>
          {bot.model ? (
            <span className="model-tag" title={`模型：${bot.model}`}>
              {bot.model}
            </span>
          ) : null}
          <HostBadge host={hostName} connected={hostUp} />
          <button
            type="button"
            className="icon-btn gear"
            aria-label="Bot 設定"
            aria-expanded={settingsOpen}
            title="Bot 設定（模型、身份、autostart、刪除）"
            onClick={() => (settingsOpen ? closeSettings() : openSettings(botId))}
          >
            ⚙
          </button>
        </div>
        <span
          className={`main-status ${lamp}`}
          title={run ? `run ${run.id}${run.pane_id ? ` ・ pane ${run.pane_id}` : ''}` : undefined}
        >
          {LAMP_LABEL[lamp]}
        </span>
        <span className="spacer" />
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
            className="mini-btn"
            disabled={!active}
            title="送出 esc 中斷目前回合"
            onClick={() => void interruptBot(botId)}
          >
            中斷
          </button>
          {active ? (
            <button
              type="button"
              className="mini-btn danger"
              disabled={Boolean(busy[`stop:${botId}`])}
              onClick={() => void stopBot(botId)}
            >
              停止
            </button>
          ) : (
            <button
              type="button"
              className="mini-btn primary"
              disabled={Boolean(busy[`start:${botId}`])}
              onClick={() => void startBot(botId)}
            >
              啟動
            </button>
          )}
        </div>
      </div>

      {settingsOpen ? (
        <BotSettingsPanel key={botId} botId={botId} />
      ) : tab === 'terminal' ? (
        active ? (
          <TerminalTab botId={botId} />
        ) : (
          <EmptyState>Bot 未在執行中，沒有可讀取的終端。</EmptyState>
        )
      ) : (
        <div className="chat">
          {blocked ? <BlockedPanel botId={botId} /> : null}
          <MessageList botId={botId} />
          <Composer botId={botId} />
        </div>
      )}
    </>
  )
}
