import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Message } from '../api/types'
import { botLamp, composerState, useStore } from '../store/store'
import { BlockedPanel } from './BlockedPanel'
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

/** A terminal-fallback reply can be a whole screen dump, so long bubbles start collapsed. */
const LONG_CHARS = 900
const LONG_LINES = 18

function isLong(content: string): boolean {
  return content.length > LONG_CHARS || content.split('\n').length > LONG_LINES
}

function Bubble({ msg }: { msg: Message }) {
  const fallback = msg.source === 'terminal_fallback'
  const long = isLong(msg.content)
  const [expanded, setExpanded] = useState(false)
  const clamped = long && !expanded

  return (
    <article className={`msg ${msg.role}`}>
      <div className={`bubble${clamped ? ' clamped' : ''}`}>
        {msg.content || <em style={{ opacity: 0.6 }}>（空白訊息）</em>}
      </div>
      {msg.role === 'system' ? null : (
        <div className="msg-meta">
          <span>{timeOf(msg.created_at)}</span>
          {msg.role === 'assistant' ? (
            <span className={`src-tag${fallback ? ' fallback' : ''}`} title={`messages.source = ${msg.source}`}>
              {SOURCE_LABEL[msg.source] ?? msg.source}
            </span>
          ) : null}
          {fallback || msg.incomplete ? <span style={{ color: 'var(--warn)' }}>可能不完整</span> : null}
          {long ? (
            <button type="button" className="link-btn" onClick={() => setExpanded(!expanded)}>
              {expanded ? '收合' : '展開全文'}
            </button>
          ) : null}
        </div>
      )}
    </article>
  )
}

function MessageList({ botId }: { botId: string }) {
  const messages = useStore((s) => s.messages[botId])
  const loaded = useStore((s) => Boolean(s.loadedBots[botId]))
  const working = useStore((s) => s.runs[botId]?.agent_status === 'working')
  const inFlight = useStore((s) => composerState(s, botId).inFlightTurnId !== null)
  const ref = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  useLayoutEffect(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [messages, working])

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
        <p className="msg-empty">
          {loaded ? '還沒有訊息。啟動 Bot 之後，在下方輸入框送出第一則訊息。' : '載入訊息中…'}
        </p>
      ) : (
        list.map((m) => <Bubble key={m.id} msg={m} />)
      )}
      {inFlight || working ? (
        <article className="msg assistant">
          <div className="bubble">
            <span className="typing">
              <i />
              <i />
              <i />
            </span>
          </div>
          <div className="msg-meta">等待回覆（hook）…</div>
        </article>
      ) : null}
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
          placeholder={state.disabled ? '目前無法送出訊息' : '輸入訊息…（Enter 送出，Shift+Enter 換行）'}
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
      <div className="composer-hint">每次送出會產生新的 client_request_id（uuid）作為冪等鍵。</div>
    </div>
  )
}

export function ChatPanel({ onOpenSidebar }: { onOpenSidebar: () => void }) {
  const botId = useStore((s) => s.selectedBotId)
  const bot = useStore((s) => s.bots.find((b) => b.id === s.selectedBotId) ?? null)
  const run = useStore((s) => (s.selectedBotId ? (s.runs[s.selectedBotId] ?? null) : null))
  const lamp = useStore((s) => (s.selectedBotId ? botLamp(s, s.selectedBotId) : 'offline'))
  const tab = useStore((s) => s.rightTab)
  const setRightTab = useStore((s) => s.setRightTab)
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
        <p className="msg-empty" style={{ margin: 'auto' }}>
          從左側選擇一個 Bot，或先新增 Project 與 Bot。
        </p>
      </>
    )
  }

  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'
  const blocked = run?.agent_status === 'blocked'

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
        </div>
        <span className="main-status">
          {LAMP_LABEL[lamp]}
          {run ? ` ・ run ${run.id.slice(-6)}${run.pane_id ? ` ・ pane ${run.pane_id}` : ''}` : ''}
        </span>
        <span className="spacer" />
        <div className="tabs" role="tablist">
          <button type="button" className="tab" role="tab" aria-selected={tab === 'chat'} onClick={() => setRightTab('chat')}>
            對話
          </button>
          <button
            type="button"
            className="tab"
            role="tab"
            aria-selected={tab === 'terminal'}
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

      {tab === 'terminal' ? (
        active ? (
          <TerminalTab botId={botId} />
        ) : (
          <p className="msg-empty" style={{ margin: 'auto' }}>
            Bot 未在執行中，沒有可讀取的終端。
          </p>
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
