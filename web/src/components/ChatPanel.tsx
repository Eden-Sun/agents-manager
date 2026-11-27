import { SendQueuedNowButton, SupplementSendNow } from './SendQueuedNowButton'
import { MessageLedgerBar } from './MessageLedgerBar'
import { TrackingBar } from './TrackingBar'
import { isApiKind } from './kindMeta'
import { terminalFooterFor } from '../hooks/useCodexTerminalFooter'
import { ApiTurnCostTag, ApiTurnProgress } from './ApiTurnCost'
import { ApiBotCostOverview } from './ApiBotCostOverview'
import { CodexTerminalFooterItems } from './CodexTerminalFooter'
import { parseCodexModelLine, splitCodexTerminalFooter } from '../lib/codexTerminalFooter'
import { projectNoFiveHour } from '../lib/quotaNoFiveHour'
import { memo, useEffect, useLayoutEffect, useRef, useState, useId, useMemo } from 'react'
import { CompactButton } from './CompactButton'
import { KeepWarmSkipButton } from './KeepWarmSkipButton'
import type { ReactNode, RefObject } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { BotKind, KindQuota, Message, QuotaWindow, StatusInfo } from '../api/types'
import { quotaKey } from '../api/types'
import { codexHeaderEffort, headerEffortLabel } from '../lib/headerEffort'
import { compactThreshold } from '../lib/contextLimit'
import { DRAWER_QUERY, PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useEnterToSend } from '../hooks/useEnterToSend'
import { useComposerFocus } from '../hooks/useComposerFocus'
import { useRefocusAfterSend } from '../hooks/useRefocusAfterSend'
import { useScrollTail } from '../hooks/useScrollTail'
import { useTapCopy } from '../hooks/useTapCopy'
import { cleanLiveActivity, cleanLiveText } from '../store/liveText'
import { typeAlongside } from '../store/alongside'
import { sendNowButton } from '../store/sendNowCopy'
import { trackDraftEdits } from '../store/draftEdits'
import { SentViaTag } from './SentViaTag'
import { startingSend, startingSendLabel } from '../store/startingSend'
import { ATTACHMENT_FAILED_NOTICE, composerPlaceholder, sendButtonLabel, sendButtonTitle } from '../lib/composerLabels'
import { queuedSendFor } from '../store/queuedSend'
import { isImeEnter } from '../lib/ime'
import { msgTimeLabel, msgTimeTitle } from '../lib/msgTime'
import { useLocalDay } from '../hooks/useLocalDay'
import { herdrIdentity } from '../lib/herdrIdentity'
import { PaneCopy } from './PaneCopy'
import { PhoneOutboxTray } from './PhoneOutboxTray'
import { anchorOf, composerState, inFlightTurn, liveReplyOf, projectHostName, toolsOfHost, useStore } from '../store/store'
import { useBotLamp } from '../hooks/useBotLamp'
import { handedOffTo } from '../lib/handoff'
import { BackgroundJobsBar } from './BackgroundJobs'
import { AttachPicker, AttachTray, DropVeil, MessageAttachments } from './Attachments'
import { useAttachments, useDropTarget } from './attachmentsHelpers'
import { BlockedBadge } from './BlockedBadge'
import { BlockedModal } from './BlockedModal'
import { BlockedPanel } from './BlockedPanel'
import { BotSettingsPanel, PersonaMark } from './BotSettingsPanel'
import { BotNameField } from './BotNameField'
import { BotSwitcher } from './BotSwitcher'
import { ConfirmDialog } from './ConfirmDialog'
import { onTabListKeyDown } from './tabKeys'
import { HostBadge } from './HostsPanel'
import { HostOfflineChatNotice } from './HostOffline'
import { UpdateBadge } from './UpdateBadge'
import { RewindButton, RewoundTag } from './RewindButton'
import { TurnErrorBadge } from './TurnErrorBadge'
import { AuthLoginAction } from './AuthLoginAction'
import { ComposerDraftBar } from './ComposerDraftBar'
import { DraftTooLargeNote } from './DraftTooLargeNote'
import { SuggestionBar } from './SuggestionBar'
import { isAuthFailure } from '../lib/authFailure'
import { parseAskAnswers } from '../lib/askAnswers'
import { AskAnswersCard } from './AskAnswersCard'
import { HostShellPanel } from './HostShellPanel'
import { GearIcon, GitIcon } from './Icons'
import { useShelfSink } from './useShelfSink'
import { IssuesBar } from './IssuesBar'
import { GitBar } from './GitBar'
import { FastBolt, KindIcon, KindTag } from './KindTag'
import { Modal } from './Modal'
import { ModelQuickPicker } from './ModelPicker'
import { RuntimeDriftBadge } from './RuntimeDriftBadge'
import { trimClippedTail } from '../lib/statusLineTail'
import { quotedFrom } from '../lib/agmQuote'
import { relaySource } from '../lib/relaySource'
import { relayPreview } from '../lib/relayPreview'
import { relayFromName } from '../lib/relayFromName'
import { pastedFiles } from '../lib/pasteFiles'
import { runningFast, runtimeIdentity, runtimeSettingsKnown } from '../lib/runtimeDrift'
import { agyEffort, shortModel } from '../lib/shortModel'
import { QuotaStrip } from './QuotaStrip'
import { PrimaryStar } from './PrimaryStar'
import { ShareLinkButton } from './ShareLinkButton'
import { UpdateQuotaChip } from './UpdateQuotaChip'
import { UnreadChip } from './UnreadChip'
import { ComposerCostHint } from './ComposerCostHint'
import { ModelCapacityNote } from './ModelCapacityNote'
import { StatusCache } from './StatusCache'
import { MobileContextPanel } from './MobileContextPanel'
import { StatusLamp } from './StatusLamp'
import { DELIVERY_HINT_TEXT, DELIVERY_WARN_TEXT, deliveryNotice } from '../lib/deliveryNotice'
import { JevNoticeText } from './JevNoticeText'
import { SafeMarkdown } from './SafeMarkdown'
import { AUTO_TRANSLATE_RECENT, useMessageTranslation } from './useMessageTranslation'
import { TerminalTab } from './TerminalTab'
import { ApiBotUsageBar } from './ApiBotUsageBar'
import { ApiBalanceInline } from './ApiBalanceInline'
import { PreviewPanel } from './PreviewPanel'
import { isTopLevelBot } from '../store/routeSync'
import { ToolsHint } from './Tools'
import { compactTokens } from '../lib/composerCost'
import { COMPACTING_LABEL } from './lampLabel'
import { GPT_61_SOL_CONTEXT_WINDOW, effectiveModelOf, sol500kContext, solWindowApplies } from '../lib/codexContextWindow'
import { ChildNoticeMessages } from './ChildNoticeMessages'
import { childNotice } from '../lib/childNotice'
import { msg, t, useT } from '../lib/i18n'
import './chatPanel.css'

const NO_QUEUED_ATTACHMENTS: Message['attachments'] = []

const SOURCE_LABEL: Record<string, string> = {
  hook: msg('回覆'),
  terminal_fallback: msg('終端擷取'),
  transcript: msg('對話紀錄'),
  web: msg('網頁訊息'),
  system: msg('系統通知'),
}

function refuseAttachments(): void {}

function systemNoticeText(content: string): string {
  const m = content.match(/^You have (\d+) usage limit resets? available\b/i)
  if (!m) return content
  const n = m[1]
  return t('Codex 還有 {n} 次額度重置可用', { n: Number(n) })
}

const RelayFrom = memo(function RelayFrom({
  fromId,
  toId,
  unverified,
  savedName,
  savedDeleted,
  cluster,
  assigneeId,
}: {
  fromId: string
  toId: string | null
  unverified: boolean
  savedName?: string | null
  savedDeleted: boolean
  cluster?: string | null
  assigneeId?: string | null
}) {
  useT()
  const names = useStore(
    useShallow((s) => ({
      from: s.bots.find((b) => b.id === fromId)?.name ?? '',
      to: toId ? (s.bots.find((b) => b.id === toId)?.name ?? '') : '',
    })),
  )
  const fromName = relayFromName({ fromId, liveName: names.from, savedName, deleted: savedDeleted })
  const label = relaySource({
    fromId,
    fromName,
    toName: names.to,
    unverified,
    cluster,
    fromIsAssignee: !!assigneeId && assigneeId === fromId,
    toIsAssignee: !!assigneeId && assigneeId === toId,
  })
  return (
    <span className={`msg-targets relay${fromName ? '' : ' daemon'}${unverified ? ' unverified' : ''}`} title={label.title}>
      {label.text}
    </span>
  )
})

interface BubbleProps {
  msg: Message
  from?: string
  fromClassName?: string
  fromTitle?: string
  kind?: BotKind
  flash?: boolean
  expandRelay?: boolean
  recent?: boolean
}

export const Bubble = memo(function Bubble(props: BubbleProps) {
  useT()
  const ask = parseAskAnswers(props.msg)
  return ask ? <AskAnswersCard msg={props.msg} ask={ask} /> : <PlainBubble {...props} />
})

const PlainBubble = memo(function PlainBubble({
  msg,
  from,
  fromClassName,
  fromTitle,
  kind,
  flash,
  expandRelay,
  recent,
}: BubbleProps) {
  const t = useT()
  const fallback = msg.source === 'terminal_fallback'
  const system = msg.role === 'system'
  const rail = system || msg.source === 'hook' || msg.source === 'system'
  const daemonNotice = msg.role === 'user' && msg.content.trimStart().startsWith('[AG Man 通知]')
  const quoted = msg.role === 'user' && !msg.relay_from ? quotedFrom(msg.content) : null
  const today = useLocalDay()
  const notice = useStore((s) =>
    msg.role === 'user' && msg.turn_id && msg.bot_id ? deliveryNotice(s.turns[msg.bot_id]?.[msg.turn_id]) : 'none',
  )
  const preview = msg.role === 'user' && (msg.relay_from || quoted) ? relayPreview(msg.content) : null
  const translation = useMessageTranslation(msg, { recent })
  const [expanded, setExpanded] = useState(false)
  const folded = Boolean(preview?.truncated) && !expanded && !expandRelay
  const tapCopy = useTapCopy(system ? systemNoticeText(msg.content) : msg.content, (ok) => {
    useStore.getState().notify(ok ? 'info' : 'error', ok ? t('已複製訊息') : t('複製失敗，請長按選取文字複製'))
  })

  return (
    <article
      className={`msg ${msg.role}${rail ? ' rail' : ''}${daemonNotice ? ' daemon-notice' : ''}${flash ? ' flash' : ''}${msg.rewound_at ? ' rewound' : ''}`}
      data-msg-id={msg.id}
      data-turn-id={msg.turn_id ?? undefined}
    >
      <div className="msg-meta msg-meta-above">
        <div className="msg-meta-left">
          {kind ? <span className={`kind-mark ${kind}`} aria-hidden="true" /> : null}
          {from ? (
            <span className={`msg-from${fromClassName ? ` ${fromClassName}` : ''}`} title={fromTitle}>
              {from}
            </span>
          ) : null}
          {msg.role === 'user' && msg.relay_from ? <RelayFrom fromId={msg.relay_from} toId={msg.bot_id ?? null} unverified={msg.relay_unverified === true} savedName={msg.relay_from_name} savedDeleted={msg.relay_from_deleted === true} cluster={msg.via_cluster} assigneeId={msg.assignee_bot_id} /> : null}
          {quoted ? (
            <span className="msg-targets relay quoted" title={t('這一則的內容來自 {source}，不是使用者自己打的', { source: quoted })}>
              {quoted}{t('（轉述）')}
            </span>
          ) : null}
          {daemonNotice ? <span className="src-tag daemon" title={t('daemon 自動通知，不是使用者直接輸入')}>{t('daemon 通知')}</span> : null}
          <SentViaTag msg={msg} />
          {notice === 'warn' ? (
            <span className="src-tag fallback" title={t.tr(DELIVERY_WARN_TEXT)}>
              {t('未驗證送達')}
            </span>
          ) : null}
          {}
          {system || msg.source === 'system' ? (
            <span className="src-tag mono" title={t('訊息來源：{source}', { source: msg.source })}>
              {SOURCE_LABEL[msg.source] ? t.tr(SOURCE_LABEL[msg.source]) : msg.source}
            </span>
          ) : msg.role === 'assistant' && msg.source !== 'hook' ? (
            <span className={`src-tag${fallback ? ' fallback' : ''}`} title={t('訊息來源：{source}', { source: msg.source })}>
              {SOURCE_LABEL[msg.source] ? t.tr(SOURCE_LABEL[msg.source]) : msg.source}
            </span>
          ) : null}
          {fallback || msg.incomplete ? <span className="meta-warn">{t('可能不完整')}</span> : null}
          {msg.rewound_at ? <RewoundTag at={msg.rewound_at} /> : null}
        </div>
        {msg.role === 'user' ? <RewindButton msg={msg} /> : null}
        {translation.button}
        {msg.role === 'assistant' && msg.bot_id ? (
          <ApiTurnCostTag
            botId={msg.bot_id}
            turnId={msg.turn_id}
          />
        ) : null}
        {}
        {system ? null : (
          <time
            className="msg-time"
            dateTime={msg.created_at}
            title={notice === 'hint' ? `${msgTimeTitle(msg.created_at)}\n${t.tr(DELIVERY_HINT_TEXT)}` : msgTimeTitle(msg.created_at)}
          >
            {msgTimeLabel(msg.created_at, today)}
          </time>
        )}
      </div>
      <div
        className={`bubble bubble-copyable${msg.role === 'assistant' && !fallback ? ' md' : ''}${rail ? ' rail' : ''}${folded ? ' folded' : ''}`}
        {...tapCopy}
      >
        {!msg.content ? (
          <em style={{ opacity: 0.6 }}>{t('（空白訊息）')}</em>
        ) : msg.role === 'assistant' && !fallback ? (
          <div key={translation.swapKey ?? (translation.flip ?? 'orig')} className={translation.animClass}>
            {translation.content ?? <SafeMarkdown text={msg.content} botId={msg.bot_id} />}
          </div>
        ) : folded && preview ? (
          preview.text
        ) : (
          system ? <JevNoticeText text={systemNoticeText(msg.content)} /> : msg.content
        )}
        {msg.attachments.length && !folded ? <MessageAttachments items={msg.attachments} /> : null}
        <TerminalSnapshot msg={msg} />
      </div>
      {}
      <SupplementSendNow msg={msg} />
      {msg.role === 'user' ? <MessageLedgerBar msg={msg} /> : null}
      {preview?.truncated && !expandRelay ? (
        <button type="button" className="disclosure sub relay-fold" aria-expanded={expanded} onClick={() => setExpanded((v) => !v)}>
          <span className="chev">{expanded ? '▼' : '▶'}</span> {expanded ? t('收合') : t('展開全文')}
          <span className="disclosure-note">
            {t('{n} 字', { n: preview.length })}{msg.attachments.length ? ` · ${t('{n} 個附件', { n: msg.attachments.length })}` : ''}
          </span>
        </button>
      ) : null}
      {isAuthFailure(msg) ? <AuthLoginAction msg={msg} /> : null}
    </article>
  )
})

function TerminalSnapshot({ msg }: { msg: Message }) {
  const t = useT()
  const [open, setOpen] = useState(false)
  if (msg.source !== 'terminal_fallback') return null
  const snap = msg.terminal_snapshot?.trim()
  if (!snap) return null
  if (snap === msg.content.trim()) return null
  return (
    <div className="msg-snapshot">
      <button type="button" className="disclosure sub" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span className="chev">{open ? '▼' : '▶'}</span> {t('完整終端畫面')}
        <span className="disclosure-note">{t('{n} 字', { n: snap.length })}</span>
      </button>
      {open ? <pre className="msg-snapshot-body">{snap}</pre> : null}
    </div>
  )
}

export function LiveBubble({
  text,
  activity,
  alert,
  from,
  kind,
  action,
  botId,
}: {
  text: string | null
  activity?: string | null
  alert?: string | null
  from?: string
  kind?: BotKind
  action?: ReactNode
  botId?: string
}) {
  const t = useT()
  const act = activity?.trim() ? activity.trim() : null
  const warn = alert?.trim() ? alert.trim() : null
  const [open, setOpen] = useState(false)
  const showText = Boolean(text) && open
  return (
    <article className={`msg assistant live${showText ? ' streaming' : ''}`}>
      <div className={`bubble${showText ? ' md' : ''}`}>
        {showText ? (
          <>
            <SafeMarkdown text={text ?? ""} botId={botId} cache={false} />
            <span className="caret" aria-hidden="true" />
          </>
        ) : (
          <TypingDots />
        )}
      </div>
      {}
      <div className="msg-meta msg-meta-below">
        <div className="msg-meta-left">
          {kind ? <span className={`kind-mark ${kind}`} aria-hidden="true" /> : null}
          {from ? <span className="msg-from">{from}</span> : null}
          <span className="sr-only" role="status">{text ? t('正在輸出回覆') : t('等待回覆')}</span>
          {}
          <span className="live-activity">{text ? (act ? `${t('輸出中…')} · ${act}` : t('輸出中…')) : (act ?? t('等待回覆…'))}</span>
          {isApiKind(kind) && botId ? <ApiTurnProgress botId={botId} /> : null}
          {text ? (
            <button type="button" className="live-peek" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
              {open ? t('收起內容') : t('看目前內容（{n} 字）', { n: text.length })}
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
  const compacting = useStore((s) => Boolean(s.runs[botId]?.compacting_since))

  if (!active) return null
  return (
    <LiveBubble
      text={text}
      activity={compacting ? COMPACTING_LABEL : activity}
      alert={alert}
      kind={kind}
      from={from}
      botId={botId}
      action={abandon ? <AbandonTurnAction botId={botId} /> : undefined}
    />
  )
}

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

export function LoadEarlier({ id, onLoad }: { id: string; onLoad: (id: string) => void }) {
  const t = useT()
  const show = useStore((s) => Boolean(s.moreMessages[id]))
  const loading = useStore((s) => Boolean(s.loadingMore[id]))
  if (!show) return null
  return (
    <div className="load-earlier">
      <button type="button" className="btn" disabled={loading} onClick={() => onLoad(id)}>
        {loading ? t('載入中…') : t('載入更早的訊息')}
      </button>
    </div>
  )
}

export function JumpToBottom({ show, onClick }: { show: boolean; onClick: () => void }) {
  const t = useT()
  if (!show) return null
  return (
    <button type="button" className="jump-bottom" title={t('捲到最新訊息')} aria-label={t('捲到最新訊息')} onClick={onClick}>
      <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
        <path d="M8 2.6v9.2M4.2 8.4L8 12.2l3.8-3.8" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" />
      </svg>
      <span>{t('最新')}</span>
    </button>
  )
}

export function LastAskPeek({ msg, turnId, onJump }: { msg: Message; turnId: string; onJump: () => void }) {
  const t = useT()
  const today = useLocalDay()
  const [ui, setUi] = useState({ turn: turnId, closed: false, open: false })
  if (ui.turn !== turnId) setUi({ turn: turnId, closed: false, open: false })
  const bodyRef = useRef<HTMLButtonElement>(null)
  const [clamped, setClamped] = useState(false)
  useLayoutEffect(() => {
    const el = bodyRef.current
    if (!el || ui.open) return
    setClamped(el.scrollHeight - el.clientHeight > 2)
  }, [msg.content, ui.open])

  if (ui.closed) return null

  const body = msg.content.trim() || (msg.attachments.length ? t('（{n} 個附件）', { n: msg.attachments.length }) : t('（空白訊息）'))
  const jumpable = !clamped || ui.open

  return (
    <div className="lastq-slot">
      <div className={`lastq${ui.open ? ' open' : ''}`} role="region" aria-label={t('這回合的提問')}>
        <div className="lastq-head">
          <span className="lastq-tag">{t('這回合的提問')}</span>
          <time className="lastq-time" dateTime={msg.created_at} title={msgTimeTitle(msg.created_at)}>
            {msgTimeLabel(msg.created_at, today)}
          </time>
          {clamped ? <span className="lastq-hint">{ui.open ? t('再點一下 · 捲到這則') : t('點一下 · 展開全文')}</span> : null}
          <button
            type="button"
            className="lastq-x"
            aria-label={t('關閉這回合的提問浮窗')}
            title={t('關閉。這一回合不會再出現，下一回合會再顯示')}
            onClick={() => setUi((u) => ({ ...u, closed: true }))}
          >
            ×
          </button>
        </div>
        <button
          ref={bodyRef}
          type="button"
          className="lastq-body"
          aria-expanded={clamped ? ui.open : undefined}
          title={jumpable ? t('捲到這則訊息') : t('展開全文')}
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

const ABANDON_AFTER_S = 60

function elapsedLabel(sec: number): string {
  if (sec < 90) return t('{n} 秒', { n: Math.floor(sec) })
  const m = Math.floor(sec / 60)
  return m < 60
    ? t('{n} 分', { n: m })
    : t('{hours} 小時 {minutes} 分', { hours: Math.floor(m / 60), minutes: m % 60 })
}

export function AbandonTurnAction({ botId }: { botId: string }) {
  const t = useT()
  const turnId = useStore((s) => inFlightTurn(s, botId)?.id ?? null)
  const createdAt = useStore((s) => inFlightTurn(s, botId)?.created_at ?? null)
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? t('這個 Bot'))
  const abandonTurn = useStore((s) => s.abandonTurn)
  const [openFor, setOpenFor] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())

  useEffect(() => {
    if (!turnId) return
    const interval = setInterval(() => setNow(Date.now()), 5000)
    return () => clearInterval(interval)
  }, [turnId])

  const startedMs = createdAt ? Date.parse(createdAt) : Number.NaN
  const elapsed = Number.isFinite(startedMs) ? (now - startedMs) / 1000 : 0

  if (!turnId || elapsed < ABANDON_AFTER_S) return null

  return (
    <>
      <button
        type="button"
        className="live-abandon"
        title={t('將這個回合標記為失敗並解開輸入框（已進行 {duration}）。Bot 可能仍在執行。', { duration: elapsedLabel(elapsed) })}
        onClick={() => setOpenFor(turnId)}
      >
        {t('強制中止 · 已 {duration}', { duration: elapsedLabel(elapsed) })}
      </button>
      <ConfirmDialog
        open={openFor === turnId}
        title={t('強制中止這個回合')}
        width={420}
        body={
          <>
            <p className="abandon-note">
              {t.x('把 {bot} 目前這個回合標記為失敗，立刻解開輸入框。適用於 Bot 已完成回覆，但畫面仍顯示等待的情況。', { bot: <strong>{botName}</strong> })}
            </p>
            <p className="abandon-note">
              {t('這個操作只更新回合紀錄，Bot 可能仍在執行。如需讓 Bot 停下來，請到「終端」分頁送出 Esc。')}
            </p>
            <p className="abandon-note">
              <strong>{t('不可逆')}</strong>{t('：之後 agent 真的回話，daemon 已經配不回這個回合，那則回覆不會出現在對話裡。')}
            </p>
          </>
        }
        confirmLabel={t('強制中止')}
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

const FLASH_MS = 1600

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
  }, [boxRef, msgId, listLen])
  return visible
}

function MessageList({ botId }: { botId: string }) {
  const t = useT()
  const messages = useStore((s) => s.messages[botId])
  const loaded = useStore((s) => Boolean(s.loadedBots[botId]))
  const failed = useStore((s) => Boolean(s.messageLoadFailed[botId]))
  const retryLoad = useStore((s) => s.loadMessages)
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
  const completedAnswered = latestCompletedTurnId !== null && list.some(
    (m) => m.role === 'assistant' && m.turn_id === latestCompletedTurnId,
  )
  const turnId = phone
    ? (!working && !inFlight && completedAnswered ? latestCompletedTurnId : null)
    : inFlightTurnId
  const recentIds = useMemo(() => new Set((messages ?? []).slice(-AUTO_TRANSLATE_RECENT).map((m) => m.id)), [messages])
  const ask = turnId ? lastAskOf(list, turnId) : null
  const lastAsk = ask && !childNotice(ask) ? ask : null
  const askVisible = useMsgVisible(tail.ref, lastAsk?.id ?? null, list.length)

  const jumpTo = (id: string): boolean => {
    const box = tail.ref.current
    const el = box?.querySelector(`[data-msg-id="${CSS.escape(id)}"]`)
    if (!box || !(el instanceof HTMLElement)) return false
    const top = el.getBoundingClientRect().top - box.getBoundingClientRect().top + box.scrollTop
    const still = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false
    box.scrollTo({ top: Math.max(0, top - 72), behavior: still ? 'auto' : 'smooth' })
    setFlashId(null)
    requestAnimationFrame(() => setFlashId(id))
    return true
  }

  return (
    <div className="msg-list-wrap">
    <TrackingBar botId={botId} onJump={jumpTo} />
    <div className="msg-list" ref={tail.ref} onScroll={tail.onScroll}>
      <LoadEarlier id={botId} onLoad={loadEarlier} />
      {failed && !loaded && list.length > 0 ? (
        <div className="load-earlier">
          <button type="button" className="btn" onClick={() => void retryLoad(botId)}>{t('先前的訊息沒載到，重試')}</button>
        </div>
      ) : null}
      {list.length === 0 && failed && !loaded ? (
        <EmptyState
          title={t('訊息載入失敗')}
          icon="!"
          action={<button type="button" className="btn" onClick={() => void retryLoad(botId)}>{t('重試')}</button>}
        >
          {t('讀不到這段對話。')}
        </EmptyState>
      ) : list.length === 0 ? (
        <EmptyState
          loading={!loaded}
          title={loaded ? t('開始交代第一個任務') : undefined}
          icon={loaded ? '✦' : undefined}
        >
          {loaded ? t('在下方輸入框寫下第一則訊息。') : t('載入訊息中…')}
        </EmptyState>
      ) : (
        <ChildNoticeMessages messages={list} renderMessage={(m, expanded) => <Bubble msg={m} flash={m.id === flashId} expandRelay={expanded} recent={recentIds.has(m.id)} />}
          liveReply={<LiveReplyBubble botId={botId} abandon />} />
      )}
      {list.length === 0 ? <LiveReplyBubble botId={botId} abandon /> : null}
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
  hideLock?: boolean
  forceFocus?: boolean
  files: ReturnType<typeof useAttachments>
}) {
  const t = useT()
  const state = useStore(useShallow((s) => composerState(s, botId)))
  const sendPrompt = useStore((s) => s.sendPrompt)
  const abandonTurn = useStore((s) => s.abandonTurn)
  const interruptBot = useStore((s) => s.interruptBot)
  const abortBot = useStore((s) => s.abortBot)
  const isClaude = useStore((s) => s.bots.find((b) => b.id === botId)?.kind === 'claude')
  const botKind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind)
  const sendNowSupported = useStore((s) => s.runs[botId]?.send_now_supported === true)
  const aborting = useStore((s) => Boolean(s.busy[`abort:${botId}`]))
  const startBot = useStore((s) => s.startBot)
  const sendText = useStore((s) => s.sendText)
  const notify = useStore((s) => s.notify)
  const queued = useStore(
    useShallow((s) => {
      const queuedSend = queuedSendFor(s, botId)
      return queuedSend
        ? { turnId: queuedSend.turnId, text: queuedSend.text, attachmentCount: queuedSend.attachments.length }
        : null
    }),
  )
  const queuedAttachments = useStore((s) =>
    queued
      ? (s.messages[botId] ?? []).find((message) => message.turn_id === queued.turnId && message.role === 'user')?.attachments ?? NO_QUEUED_ATTACHMENTS
      : NO_QUEUED_ATTACHMENTS,
  )
  const starting = useStore(useShallow((s) => startingSend(s.turns[botId], s.messages[botId])))
  const cancelStartingSend = useStore((s) => s.cancelStartingSend)
  const withdrawQueuedSend = useStore((s) => s.withdrawQueuedSend)
  const withdrawing = useStore((s) => Boolean(s.busy[`withdraw:${botId}`]))
  const startingBot = useStore((s) => Boolean(s.busy[`start:${botId}`]))
  const hasRun = useStore((s) => Boolean(s.runs[botId]))
  const draftKey = `bot:${botId}` as const
  const phone = useMediaQuery(PHONE_QUERY)
  const text = useStore((s) => s.drafts[draftKey] ?? '')
  const setDraft = useStore((s) => s.setDraft)
  const setDraftCursor = useStore((s) => s.setDraftCursor)
  const setText = (v: string) => setDraft(draftKey, v)
  const [sending, setSendingState] = useState(false)
  const ref = inputRef
  const noteSend = useRefocusAfterSend(sending, ref, phone)
  const setSending = (v: boolean) => {
    if (v) noteSend()
    setSendingState(v)
  }

  useComposerFocus({ draftKey, ref, forceFocus: forceFocus && !phone, autoFocus: !phone })

  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(200, el.scrollHeight)}px`
  }, [text, ref, phone])

  const submit = (cacheTtl?: '5m' | '1h') => {
    const body = text.trim()
    if (!body && files.ids.length === 0) return
    if (state.disabled) {
      notify('error', state.reason || t('目前無法送出訊息'))
      return
    }
    if (sending || files.uploading) return
    if (files.failed) {
      notify('error', t.tr(ATTACHMENT_FAILED_NOTICE))
      return
    }
    setSending(true)
    const sent = files.ids
    const edits = trackDraftEdits(draftKey)
    const editing = useStore.getState().composerDrafts[botId]
    const editRequest = editing?.editing && editing.token && editing.actions.includes('clear') ? ({ action: 'clear', token: editing.token } as const) : undefined
    void sendPrompt(botId, body, sent, false, editRequest ? false : Boolean(state.autoStart), editRequest, editRequest ? false : true, botKind === 'claude-api' ? (cacheTtl ?? '5m') : undefined).then((ok) => {
      setSending(false)
      if (ok) {
        if (edits.untouched()) setText('')
        files.clearSent(sent)
      }
    }).finally(edits.stop)
  }
  const enterToSend = useEnterToSend()

  const pending = text

  const sendNow = async () => {
    const body = pending.trim()
    const ids = files.ids
    if (!body && ids.length === 0) return
    if (sending) return
    if (files.uploading) {
      notify('error', t('附件還在上傳，傳完再送'))
      return
    }
    if (files.failed) {
      notify('error', t.tr(ATTACHMENT_FAILED_NOTICE))
      return
    }
    setSending(true)
    const edits = trackDraftEdits(draftKey)
    const ok = await sendPrompt(botId, body, ids, true).finally(edits.stop)
    setSending(false)
    if (ok) {
      if (edits.untouched()) setText('')
      files.clearSent(ids)
    }
  }

  const sendAlongside = async () => {
    const body = pending.trim()
    if (!body) return
    setSending(true)
    const edits = trackDraftEdits(draftKey)
    const ok = await typeAlongside({ sendText }, botId, body).finally(edits.stop)
    setSending(false)
    if (ok && edits.untouched()) setText('')
  }

  const withdrawQueuedIdleSend = async () => {
    if (!queued) return
    await withdrawQueuedSend(botId)
    const withdrawn = useStore.getState().turns[botId]?.[queued.turnId]?.status === 'failed'
    if (withdrawn) files.restoreAttachments(queuedAttachments)
  }

  const showLock = !hideLock && Boolean(state.reason) && (state.disabled || Boolean(state.inFlightTurnId))

  const nothingToSend = !text.trim() && files.ids.length === 0

  const syncCursor = () => {
    const el = ref.current
    if (el) setDraftCursor(draftKey, el.selectionStart, el.selectionEnd)
  }

  return (
    <div className="composer bot-composer">
      <ComposerDraftBar
        botId={botId}
        text={text}
        attachments={files.ids}
        attachmentsBlocked={files.uploading ? 'uploading' : files.failed ? 'failed' : null}
        onSent={(textUntouched) => {
          if (textUntouched) setText('')
          files.clear()
        }}
        onEdit={(draft) => {
          const cur = useStore.getState().drafts[draftKey] ?? ''
          setText(cur.trim() ? `${draft}\n${cur}` : draft)
          setDraftCursor(draftKey, draft.length)
          ref.current?.focus()
        }}
      />
      <SuggestionBar botId={botId} attachments={files.ids.length} />
      {starting ? (
        <div className="composer-queued" role="status">
          <span className="composer-queued-label">{startingSendLabel(starting, hasRun)}</span>
          <span className="composer-queued-text" title={starting.text}>
            {starting.text || t('（{n} 個附件）', { n: starting.attachments })}
          </span>
          {starting.startError && !hasRun ? (
            <button type="button" className="mini-btn" disabled={startingBot} title={t('再啟動一次，起來後照樣自動送出')} onClick={() => void startBot(botId)}>
              {t('重新啟動')}
            </button>
          ) : null}
          <button
            type="button"
            className="mini-btn"
            title={t('不送了，把訊息放回輸入框')}
            onClick={() => void cancelStartingSend(botId)}
          >
            {t('取消')}
          </button>
        </div>
      ) : null}
      {queued ? (
        <div className="composer-queued composer-queued-idle" role="status">
          <span className="composer-queued-label">{t('已排隊，Bot 這回合結束後會自動送出：')}</span>
          <span className="composer-queued-text" title={queued.text}>
            {queued.text
              ? queued.attachmentCount
                ? t('{text}（附 {n} 個檔案）', { text: queued.text, n: queued.attachmentCount })
                : queued.text
              : t('（{n} 個附件）', { n: queued.attachmentCount })}
          </span>
          <button
            type="button"
            className="mini-btn"
            disabled={withdrawing}
            title={t('撤回這則訊息並放回輸入框')}
            onClick={() => void withdrawQueuedIdleSend()}
          >
            {withdrawing ? t('撤回中…') : t('撤回並放回')}
          </button>
        </div>
      ) : null}
      {showLock ? (
        <div className={`composer-lock${state.disabled ? '' : ' running'}`} role="status">
          <span title={state.reason}>{phone && !state.disabled && state.inFlightTurnId ? t('執行中 · 送出排隊') : state.reason}</span>
          {}
          {state.unknownTurnId && !state.unknownWorking ? (
            <button type="button" className="mini-btn" onClick={() => void abandonTurn(botId, state.unknownTurnId!)}>
              {t('放棄該回合')}
            </button>
          ) : null}
          {state.inFlightTurnId ? (
            <button
              type="button"
              className="mini-btn"
              title={isApiKind(botKind) ? t('立刻中止進行中的 API 請求並結束這一回合（不再繼續花費），Bot 仍保持啟動') : t('送一個 Esc 請 Bot 中斷目前回覆，Bot 仍保持啟動')}
              onClick={() => void interruptBot(botId)}
            >
              {isApiKind(botKind) ? t('強制中斷') : t('ESC 中斷')}
            </button>
          ) : null}
          {state.inFlightTurnId && pending.trim() && (isClaude || sendNowSupported) && sendNowButton(botKind, pending) ? (
            <>
              {}
              {sendNowSupported ? <button
                type="button"
                className="mini-btn"
                disabled={aborting || sending || files.uploading}
                title={files.uploading ? t('附件上傳中…') : sendNowButton(botKind, pending)!.title}
                onClick={() => void sendNow()}
              >
                {sendNowButton(botKind, pending)!.label}
              </button> : null}
              {}
              {isClaude ? (
                <button
                  type="button"
                  className="mini-btn"
                  disabled={sending}
                  title={t('補充一句給正在跑的這一輪：不建立新回合，直接打進終端，它邊做邊看到。這句會記在對話裡（標「補充」），回覆併在目前這一輪。')}
                  onClick={() => void sendAlongside()}
                >
                  {t('補充')}
                </button>
              ) : null}
            </>
          ) : null}
          <SendQueuedNowButton botId={botId} inFlightTurnId={state.inFlightTurnId} inputEmpty={!pending.trim()} />
          {}
          {state.inFlightTurnId || state.unknownTurnId ? (
            <button
              type="button"
              className="mini-btn danger"
              disabled={aborting}
              title={t('對話卡住、送不出字時用這顆：不等 agent 回應，直接把這回合標成失敗並解開輸入框。Bot 仍保持啟動——它那頭可能還在跑。')}
              onClick={() => void abortBot(botId)}
            >
              {aborting ? t('解除中…') : t('解除卡對話')}
            </button>
          ) : null}
        </div>
      ) : null}
      <ModelCapacityNote botId={botId} />
      <ComposerCostHint botId={botId} />
      <AttachTray items={files.items} onRemove={files.remove} onRetry={files.retry} disabled={sending} />
      <DraftTooLargeNote draftKey={draftKey} />
      <div className="composer-box">
        {}
        {botKind === 'openai-api' ? null : <AttachPicker onFiles={files.add} disabled={state.disabled || sending} />}
        <textarea
          ref={ref}
          data-draft-key={draftKey}
          rows={phone ? 1 : 2}
          value={text}
          disabled={sending}
          placeholder={composerPlaceholder(state, { phone, starting: Boolean(starting) })}
          title={t('Enter 送出，Shift+Enter 換行；檔案可拖放或貼上')}
          onChange={(e) => {
            setText(e.target.value)
            setDraftCursor(draftKey, e.target.selectionStart, e.target.selectionEnd)
          }}
          onSelect={syncCursor}
          onClick={syncCursor}
          onBlur={syncCursor}
          onKeyUp={syncCursor}
          onPaste={(e) => {
            const pasted = pastedFiles(e.clipboardData)
            if (pasted.length === 0) return
            e.preventDefault()
            files.add(pasted)
          }}
          onKeyDown={(e) => {
            if (enterToSend.enterSends && e.key === 'Enter' && !e.shiftKey && !isImeEnter(e.nativeEvent)) {
              e.preventDefault()
              submit()
            }
          }}
          {...enterToSend.props}
        />
        {botKind === 'claude-api' ? (
          <div className="send-btns" role="group" aria-label={t('送出')}>
            {(['5m', '1h'] as const).map((ttl) => {
              const idle = !sending && !files.uploading && !files.failed
              return (
                <button
                  key={ttl}
                  type="button"
                  className={`send-btn${ttl === '1h' ? ' long' : ''}`}
                  data-ttl={ttl}
                  disabled={state.disabled || sending || files.uploading || files.failed || nothingToSend}
                  title={ttl === '5m' ? t('快取保留 5 分鐘（Enter 也是這個）：寫入較便宜') : t('快取保留 1 小時：寫入貴 60%，適合下一次提問隔 5–60 分鐘；問完就走選 5 分鐘')}
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => submit(ttl)}
                >
                  {idle ? (
                    phone ? (
                      <span className="send-btn-label">
                        <span className="send-btn-action">{t('送出')}</span>
                        <span className="send-btn-ttl">{ttl === '5m' ? t('(5 分)') : t('(1 小時)')}</span>
                      </span>
                    ) : (
                      ttl === '5m' ? t('送出（快取 5 分）') : t('送出（快取 1 小時）')
                    )
                  ) : (
                    sendButtonLabel(state, { phone, sending, uploading: files.uploading, failed: files.failed })
                  )}
                </button>
              )
            })}
          </div>
        ) : (
          <button
            type="button"
            className="send-btn"
            disabled={state.disabled || sending || files.uploading || files.failed || nothingToSend}
            title={sendButtonTitle(state, files.uploading, files.failed)}
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => submit()}
          >
            {sendButtonLabel(state, { phone, sending, uploading: files.uploading, failed: files.failed })}
          </button>
        )}
      </div>
    </div>
  )
}

function pct(n: number): string {
  const r = Math.round(n * 10) / 10
  return `${Number.isInteger(r) ? r : r.toFixed(1)}%`
}

function contextUsedShort(detail: string | null): string | null {
  if (!detail) return null
  const slash = detail.indexOf('/')
  const used = (slash === -1 ? detail : detail.slice(0, slash)).trim()
  return used || null
}

function SlItem({ k, children, title, className }: { k: string; children: ReactNode; title?: string; className?: string }) {
  return (
    <span className={`sl-item${className ? ' ' + className : ''}`} title={title} data-k={k}>
      <span className="sl-k">{k}</span>
      <span className="sl-v">{children}</span>
    </span>
  )
}

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
  quota = projectNoFiveHour(kind, quota)
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
    context_unknown: false,
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

function modelExtraOf(effort: string | null, fast: boolean): string {
  return [effort ? headerEffortLabel(effort) : null, fast ? 'fast' : null]
    .filter(Boolean)
    .join(' · ')
}

function ContextBar({ issues, status, hasStatus, mobileContent, mobileOpen, onOpen, onClose, mobileStatus }: {
  issues: ReactNode
  status: ReactNode
  hasStatus: boolean
  mobileContent: ReactNode
  mobileOpen: boolean
  onOpen: () => void
  onClose: () => void
  mobileStatus?: boolean
}) {
  const t = useT()
  const phone = useMediaQuery(PHONE_QUERY)
  if (!issues && !hasStatus) return null
  if (phone) {
    return (
      <>
        {mobileOpen ? mobileContent : null}
        {mobileStatus && hasStatus ? <div className="context-bar mobile-api-context-bar">{status}</div> : null}
      </>
    )
  }
  return (
    <>
      <div className="context-bar">
        {issues ? <button type="button" className="mini-btn desktop-git-info" aria-label={t('Git / 專案資訊')} aria-haspopup="dialog" aria-expanded={mobileOpen} onClick={onOpen}><GitIcon /> GitHub</button> : null}
        {hasStatus ? status : null}
      </div>
      {issues && mobileOpen ? <Modal open title={t('Git / 專案資訊')} onClose={onClose}><div className="context-bar">{issues}</div></Modal> : null}
    </>
  )
}

function useContextReading(botId: string, status: StatusInfo | null, text: string | null) {
  const t = useT()
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind)
  const effectiveModel = useStore((s) => {
    const r = s.runs[botId]
    return effectiveModelOf(r?.runtime_model, s.bots.find((b) => b.id === botId)?.model)
  })
  const runPromptCache = useStore((s) => s.runs[botId]?.prompt_cache ?? null)
  const threshold = useStore((s) => {
    const b = s.bots.find((x) => x.id === botId)
    return compactThreshold(b, b ? s.projects.find((p) => p.id === b.project_id) : undefined)
  })
  const line = text?.trim() ?? ''
  const footer = useStore((s) => terminalFooterFor(s, botId)) ?? (kind === 'codex' && line ? splitCodexTerminalFooter(line).footer : null)
  const fields = useMemo(() => (footer?.modelLine ? parseCodexModelLine(footer.modelLine) : null), [footer?.modelLine])
  const solOn = solWindowApplies(kind, effectiveModel)
  const solCtx = solOn ? sol500kContext(kind, effectiveModel, runPromptCache?.context_used_tokens ?? status?.context_used_tokens) : null
  const usedTokens = runPromptCache?.context_used_tokens ?? status?.context_used_tokens ?? null
  const windowSize = solOn ? GPT_61_SOL_CONTEXT_WINDOW : (status?.context_size ?? runPromptCache?.context_size ?? null)
  const limit = threshold !== null && usedTokens !== null && !status?.context_unknown && (windowSize === null || threshold < windowSize) ? threshold : null
  const ctxPct = limit !== null
    ? (usedTokens! / limit) * 100
    : solOn
      ? (solCtx?.pct ?? null)
      : status?.context_unknown
        ? null
        : (fields?.contextUsedPct ?? status?.context_used_pct ?? (usedTokens !== null && windowSize !== null && windowSize > 0 ? (usedTokens / windowSize) * 100 : null))
  const ctxDetail = limit !== null
    ? `${compactTokens(usedTokens!)}/${compactTokens(limit)}`
    : solOn
      ? solCtx ? `${compactTokens(solCtx.tokens)}/${compactTokens(GPT_61_SOL_CONTEXT_WINDOW)}` : null
      : status && status.context_used_tokens !== null && status.context_size !== null
        ? `${compactTokens(status.context_used_tokens)}/${compactTokens(status.context_size)}`
        : !status?.context_unknown && usedTokens !== null && windowSize !== null && windowSize > 0
          ? `${compactTokens(usedTokens)}/${compactTokens(windowSize)}`
          : null
  const ctxTitle = limit !== null
    ? t('已用 {detail} tokens；到 {limit} 自動壓縮（模型視窗 {window}）', { detail: ctxDetail ?? '', limit: compactTokens(limit), window: windowSize !== null ? compactTokens(windowSize) : '—' })
    : ctxDetail ? t('已用 {detail} tokens', { detail: ctxDetail }) : undefined
  return { kind, line, footer, fields, pct: status?.context_unknown ? null : ctxPct, detail: status?.context_unknown ? null : ctxDetail, title: ctxTitle, unknown: Boolean(status?.context_unknown) }
}

export function StatusLineBar({ botId, status, text }: { botId: string; status: StatusInfo | null; text: string | null }) {
  const t = useT()
  const phone = useMediaQuery(PHONE_QUERY)
  const { kind, line, footer, fields, pct: ctxPct, detail: ctxDetail, title: ctxTitle } = useContextReading(botId, status, text)
  if (!status && !footer) {
    if (!line && kind !== 'codex') return null
    return (
      <div className="statusline-bar" title={line || undefined}>
        {}
        {line ? <span className="statusline-text mono">{trimClippedTail(line)}</span> : null}
        {kind === 'codex' ? <StatusCache botId={botId} status={null} contextAction /> : null}
      </div>
    )
  }

  const cwd = fields?.cwd ?? status?.cwd ?? null
  return (
    <div className="statusline-bar" title={line || footer?.modelLine || undefined}>
      <CodexTerminalFooterItems footer={footer} />
      {status?.account_warning ? (
        <SlItem k={t('帳號')} className="sl-account sl-warn" title={status.account_warning}>
          {t('⚠ 未登入，用的是預設帳號')}
        </SlItem>
      ) : status?.account_email ? (
        <SlItem k={t('帳號')} className="sl-account">
          {status.account_email}
        </SlItem>
      ) : cwd ? (
        <SlItem k={t('目錄')} title={status?.cwd ?? cwd}>
          {cwd.replace(/^\/Users\/[^/]+/, '~')}
        </SlItem>
      ) : null}
      {ctxPct !== null ? (
        <SlItem k="context" title={ctxTitle}>
          {pct(ctxPct)}{ctxDetail ? <span className="sl-dim"> · {ctxDetail}</span> : null}
          <CompactButton botId={botId} />
          {}
          {phone ? null : <KeepWarmSkipButton botId={botId} />}
        </SlItem>
      ) : status?.context_unknown ? (
        <SlItem k="context" title={t('剛 resume／壓縮，等下一回合更新')}>—</SlItem>
      ) : null}
      {status || (ctxPct === null && kind === 'codex') ? <StatusCache botId={botId} status={status} contextAction={ctxPct === null && kind === 'codex'} /> : null}
      {}
      {status?.version ? (
        <SlItem k={t('版本')} className="sl-version">
          {status.version}
        </SlItem>
      ) : null}
      <UpdateBadge botId={botId} variant="inline" />
    </div>
  )
}

const AUTO_OPEN_DELAY_MS = 1000

export function ChatPanel({ onOpenSidebar }: { onOpenSidebar: () => void }) {
  const t = useT()
  const botId = useStore((s) => s.selectedBotId)
  const bot = useStore((s) => s.bots.find((b) => b.id === s.selectedBotId) ?? null)
  const run = useStore((s) => (s.selectedBotId ? (s.runs[s.selectedBotId] ?? null) : null))
  const terminalFooter = useStore((s) => terminalFooterFor(s, botId))
  const runningIdentity = runtimeIdentity(run)
  const headUnread = useStore((s) => (s.selectedBotId ? (s.botUnread[s.selectedBotId] ?? 0) : 0))
  const phone = useMediaQuery(PHONE_QUERY)
  const { lamp, background: bgJobs, label: stateLabel, kids: kidsRunning } = useBotLamp(botId)
  const hostName = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null))
  const hostUp = useStore((s) => {
    const name = projectHostName(s, s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null)
    return name === 'local' || (s.hosts.find((h) => h.name === name)?.connected ?? false)
  })
  const rawTab0 = useStore((s) => s.rightTab)
  const apiBot = useStore((s) => isApiKind(s.bots.find((b) => b.id === s.selectedBotId)?.kind))
  const rawTab = apiBot && rawTab0 === 'terminal' ? 'chat' : rawTab0
  const drawerLayout = useMediaQuery(DRAWER_QUERY)
  const canPreview = drawerLayout && isTopLevelBot(bot ?? undefined)
  const tab = rawTab === 'preview' && !canPreview ? 'chat' : rawTab
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
  const dockRef = useRef<HTMLDivElement>(null)
  useEffect(() => {
    const el = dockRef.current
    if (!el) return
    const apply = () => {
      const h = Math.round(el.getBoundingClientRect().height)
      if (h > 0) {
        document.documentElement.style.setProperty('--composer-bottom-offset', `${h}px`)
      }
    }
    apply()
    const ro = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(apply)
    ro?.observe(el)
    return () => {
      ro?.disconnect()
      document.documentElement.style.removeProperty('--composer-bottom-offset')
    }
  }, [botId])
  const mobileContextAnchor = useRef<HTMLButtonElement>(null)
  const [gitInfoBotId, setGitInfoBotId] = useState<string | null>(null)
  const [costOverviewOpen, setCostOverviewOpen] = useState(false)
  const jumpToTurn = (turnId: string) => {
    const el = document.querySelector(`article[data-turn-id="${turnId}"]`)
    if (el) {
      el.scrollIntoView({ behavior: 'smooth', block: 'center' })
      el.classList.add('flash')
      setTimeout(() => el.classList.remove('flash'), 2000)
    }
  }
  const statusInfo = useStore(
    useShallow((s): StatusInfo | null => {
      const b = s.bots.find((x) => x.id === s.selectedBotId)
      if (!b) return null
      const r = s.runs[b.id] ?? null
      if (r?.status && b.kind !== 'agy') return r.status
      if (b.kind === 'claude') return null
      const host = projectHostName(s, b.project_id)
      const liveIdentity = runtimeIdentity(r)
      const identity = liveIdentity === undefined ? b.identity : liveIdentity
      const key = quotaKey(host, identity ? `${b.kind}:${identity}` : b.kind)
      const q = s.quota[key] ?? s.quota[quotaKey(host, b.kind)] ?? null
      const tool = toolsOfHost(s, host)[b.kind]
      const path = s.projects.find((p) => p.id === b.project_id)?.path ?? null
      const live = runtimeSettingsKnown(r)
      const derived = derivedStatus(
        b.kind,
        live ? r!.runtime_model : b.model,
        live ? r!.runtime_effort : b.effort,
        live ? (r!.runtime_fast ?? b.fast) : b.fast,
        path,
        q,
        tool?.version ?? null,
      )
      return b.kind === 'agy' && r?.status && derived
        ? { ...derived, context_used_pct: r.status.context_used_pct, context_used_tokens: r.status.context_used_tokens, context_size: r.status.context_size }
        : derived
    }),
  )
  const slugEffort = bot ? agyEffort(bot.kind, bot.model ?? statusInfo?.model_name ?? null) : null
  const statusLineModelLine = bot?.kind === 'codex' && run?.state === 'running' && run.status_line
    ? splitCodexTerminalFooter(run.status_line).footer?.modelLine ?? null
    : null
  const codexRunning = bot?.kind === 'codex' && run?.state === 'running'
  const runEffort = codexRunning ? (runtimeSettingsKnown(run) ? run!.runtime_effort : bot?.effort ?? null) : null
  const headerEffort = codexRunning
    ? codexHeaderEffort({
        terminalModelLine: terminalFooter?.modelLine ?? null,
        statusEffort: statusInfo?.effort ?? null,
        statusLineModelLine,
        runEffort,
        slugEffort,
        parse: parseCodexModelLine,
      })
    : (statusInfo?.effort ?? slugEffort)
  const fast = bot ? runningFast(bot, run) : false
  const modelExtra = modelExtraOf(headerEffort, fast)
  const codexFast = bot?.kind === 'codex' && runningFast(bot, run)
  const herdrId = herdrIdentity(run, bot?.agent_name)
  const noAttach = bot?.kind === 'openai-api'
  const attachments = useAttachments(botId, botId)
  const files = noAttach ? { ...attachments, add: refuseAttachments } : attachments
  const drop = useDropTarget(files.add, !botId)
  useShelfSink(files.add, botId && bot && (tab === 'chat' || settingsBotId === botId) ? bot.name : null)

  const messages = useStore((s) => (botId ? s.messages[botId] : undefined))
  const messagesLoaded = useStore((s) => (botId ? Boolean(s.loadedBots[botId]) : false))
  const chatEmpty = messagesLoaded && (messages?.length ?? 0) === 0
  const composerReason = useStore((s) => (botId ? composerState(s, botId).reason : ''))
  const handedOff = useStore((s) => handedOffTo(s.projects, s.bots.find((b) => b.id === botId)?.project_id) != null)
  const waitingForStart = useStore((s) => (botId ? startingSend(s.turns[botId], s.messages[botId]) !== null : false))

  const blockedNow = run?.agent_status === 'blocked'
  const [blockedUi, setBlockedUi] = useState({ bot: botId ?? '', blocked: false, armed: false, open: false, dismissed: false })
  if (blockedUi.bot !== (botId ?? '') || blockedUi.blocked !== blockedNow) {
    const sameBot = blockedUi.bot === (botId ?? '')
    const dismissed = sameBot && blockedNow ? blockedUi.dismissed : false
    const armed = blockedNow && !dismissed
    setBlockedUi({ bot: botId ?? '', blocked: blockedNow, armed, open: armed && blockedUi.open, dismissed })
  }
  useEffect(() => {
    if (!blockedUi.armed) return
    const t = setTimeout(() => setBlockedUi((u) => (u.armed ? { ...u, armed: false, open: true } : u)), AUTO_OPEN_DELAY_MS)
    return () => clearTimeout(t)
  }, [blockedUi.armed])
  const blockedFull = blockedUi.open
  const phoneCtx = useContextReading(botId ?? '', statusInfo, run?.status_line ?? null)
  const phoneUsed = phoneCtx.unknown ? null : contextUsedShort(phoneCtx.detail)
  const phonePctLabel = phoneCtx.pct !== null ? `${Math.round(phoneCtx.pct)}%` : null
  const phoneBoth = phoneUsed !== null && phonePctLabel !== null
  const phoneHasCtx = phonePctLabel !== null || phoneCtx.unknown || phoneUsed !== null
  const phoneAriaCtx = phoneBoth
    ? ` · context ${phoneUsed} · ${phonePctLabel}`
    : phonePctLabel
      ? ` · context ${pct(phoneCtx.pct!)}`
      : phoneUsed
        ? ` · context ${phoneUsed}`
        : ''

  if (!botId || !bot) {
    return (
      <>
        <div className="main-head">
          <button
            type="button"
            className="btn menu-btn icon-tip"
            onClick={onOpenSidebar}
            aria-label={t('開啟側邊欄')}
            data-tip={t('開啟側邊欄')}
          >
            ☰
          </button>
          <span className="main-status">{t('未選擇 Bot')}</span>
          <span className="spacer" />
          <QuotaStrip />
        </div>
        <UnreadChip />
        <EmptyState title={t('尚未選擇 Bot')} icon="◎">
          {t('從左側選擇一個 Bot，或先新增 Project 與 Bot。')}
        </EmptyState>
      </>
    )
  }

  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'
  const blocked = blockedNow
  const settingsOpen = settingsBotId === botId
  const shellOpen = shellView !== null && !settingsOpen
  const closeBlockedFull = () => setBlockedUi((u) => ({ ...u, armed: false, open: false, dismissed: true }))
  const openBlockedFull = () => setBlockedUi((u) => ({ ...u, armed: false, open: true }))

  return (
    <>
      <div className="main-head bot-head">
        <button
          type="button"
          className="btn menu-btn icon-tip"
          onClick={onOpenSidebar}
          aria-label={t('開啟側邊欄')}
          data-tip={t('開啟側邊欄')}
        >
          ☰
        </button>
        <div className="main-title">
          <div className="main-title-row">
            <StatusLamp lamp={lamp} background={bgJobs} kids={kidsRunning} />
            {}
            {phone ? <BotSwitcher botId={botId} name={bot.name} /> : <BotNameField botId={botId} name={bot.name} />}
            {}
            {phone ? <UpdateQuotaChip /> : null}
            {}
            <PrimaryStar botId={botId} />
            <button
              type="button"
              className="icon-btn gear icon-tip"
              aria-label={t('{name} 的設定', { name: bot.name })}
              aria-expanded={settingsOpen}
              data-tip={t('設定 · {name}', { name: bot.name })}
              onClick={(e) => (settingsOpen ? closeSettings() : openSettings(botId, anchorOf(e.currentTarget)))}
            >
              <GearIcon />
            </button>
            <ShareLinkButton botId={botId} />
            <PersonaMark persona={bot.persona} />
            <HostBadge host={hostName} connected={hostUp} />
            <UpdateBadge botId={botId} />
            <RuntimeDriftBadge botId={botId} />
            {headUnread > 0 ? (
              <span className="unread-turns" title={t('{n} 個回合已完成，還沒看過', { n: headUnread })}>
                !{headUnread > 99 ? '99+' : headUnread}
              </span>
            ) : null}
          </div>
          {}
          {phone ? (
            <div className="mobile-bot-version" title={t('{kind} · {version}', { kind: bot.kind, version: statusInfo?.version ?? t('版本未回報') })}>
              <span className={`mobile-kind-icon ${bot.kind}${codexFast ? ' fast' : ''}`} role="img" aria-label={codexFast ? t('{kind}（fast）', { kind: bot.kind }) : bot.kind}><KindIcon kind={bot.kind} />{codexFast ? <FastBolt /> : null}</span>
              {bot.model || statusInfo?.model_name ? (
                <span className="mobile-bot-model">
                  {shortModel(bot.kind, bot.model ?? statusInfo?.model_name ?? null)}
                  {headerEffort ? ` · ${headerEffortLabel(headerEffort)}` : ''}
                </span>
              ) : null}
              {apiBot ? <ApiBalanceInline /> : null}
              <span className="mobile-bot-ver">{statusInfo?.version?.match(/\d+\.\d+\.\d+(?:[-+][\w.-]+)?/)?.[0] ?? statusInfo?.version ?? '—'}</span>
              <BlockedBadge botId={botId} onOpen={openBlockedFull} />
              {}
              <TurnErrorBadge botId={botId} />
            </div>
          ) : <div className="main-title-sub">
            {}
            <BlockedBadge botId={botId} onOpen={openBlockedFull} />
            {}
            <TurnErrorBadge botId={botId} />
            <KindTag kind={bot.kind} fast={codexFast} />
            {apiBot ? (
              <span className={`model-tag${bot.fast ? ' fast' : ''}`} title={t('claude-api 固定用 {model}', { model: bot.model ?? '' })}>
                {shortModel(bot.kind, bot.model)}
                {bot.effort ? <span className="model-tag-extra">{` · ${headerEffortLabel(bot.effort)}${bot.fast ? ' · fast' : ''}`}</span> : null}
              </span>
            ) : bot.model || statusInfo?.model_name ? (
              <ModelQuickPicker
                botId={botId}
                kind={bot.kind}
                host={hostName}
                className={`model-tag${bot.model ? '' : ' reported'}`}
                title={
                  bot.model
                    ? t('點一下改模型（{model}）', { model: bot.model })
                    : t('CLI 預設，實際載入 {model}。點一下改模型', { model: statusInfo?.model_name ?? '' })
                }
              >
                {shortModel(bot.kind, bot.model ?? statusInfo?.model_name ?? null)}
                {modelExtra ? <span className="model-tag-extra">{modelExtra}</span> : null}
              </ModelQuickPicker>
            ) : null}
            {}
            {run?.pane_id && !apiBot ? <PaneCopy paneId={run.pane_id} detail={t('{pane}（{status}）', { pane: herdrId?.title ?? `pane ${run.pane_id}`, status: stateLabel })} /> : null}
            {}
            {apiBot ? <PaneCopy what="bot" paneId={bot.id} detail={t('Claude API bot（{status}）', { status: stateLabel })} /> : null}
          </div>}
        </div>
        {phone ? (
          <>
            <button
              type="button"
              ref={mobileContextAnchor}
              className={`icon-btn mobile-git-info${statusInfo?.account_warning ? ' warn' : ''}${phoneHasCtx ? ' has-ctx' : ''}${phoneBoth ? ' has-both' : ''}`}
              aria-label={`${statusInfo?.account_warning ? t('Context 與專案（帳號警告）') : t('Context 與專案')}${phoneAriaCtx}`}
              title={phoneCtx.title}
              aria-haspopup="dialog"
              aria-expanded={gitInfoBotId === botId}
              onClick={() => setGitInfoBotId((open) => open === botId ? null : botId)}
            >
              {phoneBoth ? (
                <span className="mobile-ctx" aria-hidden="true">
                  <span className="mobile-ctx-k">{phoneUsed}</span>
                  <span className="mobile-ctx-pct">{phonePctLabel}</span>
                </span>
              ) : (
                <>
                  <GitIcon />
                  {phonePctLabel ? (
                    <span className="mobile-ctx" aria-hidden="true">{phonePctLabel}</span>
                  ) : phoneUsed ? (
                    <span className="mobile-ctx" aria-hidden="true">{phoneUsed}</span>
                  ) : phoneCtx.unknown ? (
                    <span className="mobile-ctx" aria-hidden="true">—</span>
                  ) : null}
                </>
              )}
            </button>
            {}
            <div className="mobile-primary-row" />
          </>
        ) : null}
        <span className="spacer" />
        <QuotaStrip
          focusKind={bot.kind}
          focusIdentity={runningIdentity === undefined ? bot.identity : runningIdentity}
          host={hostName}
        />
        {}
        <div className="tabs" role="tablist" aria-label={t('主面板')} onKeyDown={onTabListKeyDown}>
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
            {t('對話')}
          </button>
          {apiBot ? null : <button
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
            {t('終端')}
          </button>}
          {canPreview ? (
            <button
              type="button"
              className="tab"
              role="tab"
              id={`${panelId}-tab-preview`}
              aria-controls={`${panelId}-panel`}
              aria-selected={tab === 'preview' && !settingsOpen && !shellOpen}
              onClick={() => {
                closeShellView()
                setRightTab('preview')
              }}
            >
              {t('預覽')}
            </button>
          ) : null}
          {shellOpen ? (
            <button
              type="button"
              className="tab"
              role="tab"
              id={`${panelId}-tab-shell`}
              aria-controls={`${panelId}-panel`}
              aria-selected
              title={t('主機 shell（按「關閉」回到對話）')}
            >
              shell
            </button>
          ) : null}
        </div>
      </div>
      <UnreadChip />
      <ToolsHint focusHost={hostName} focusKinds={[bot.kind]} />
      {}
      <ContextBar
        mobileStatus={apiBot}
        mobileContent={<MobileContextPanel botId={botId} projectId={bot.project_id} status={statusInfo} reading={phoneCtx}
          anchor={mobileContextAnchor} onClose={() => setGitInfoBotId(null)}
          issues={<IssuesBar key={bot.project_id} projectId={bot.project_id} draftKey={`bot:${botId}`} inputRef={composerRef} onInserted={() => setGitInfoBotId(null)} />} />}
        mobileOpen={gitInfoBotId === botId}
        onOpen={() => setGitInfoBotId(botId)}
        onClose={() => setGitInfoBotId(null)}
        issues={
          phone || (tab === 'chat' && !settingsOpen) ? (
            <>
              <IssuesBar
                key={bot.project_id}
                projectId={bot.project_id}
                draftKey={`bot:${botId}`}
                inputRef={composerRef}
                onInserted={() => setGitInfoBotId(null)}
              />
              <GitBar projectId={bot.project_id} />
            </>
          ) : null
        }
        status={
          apiBot ? (
            <ApiBotUsageBar
              botId={botId}
              onToggleOverview={() => setCostOverviewOpen((v) => !v)}
              overviewOpen={costOverviewOpen}
            />
          ) : (
            <StatusLineBar botId={botId} status={statusInfo} text={run?.status_line ?? null} />
          )
        }
        hasStatus={apiBot || bot.kind === 'codex' || Boolean(statusInfo) || Boolean(run?.status_line?.trim()) || Boolean(terminalFooter)}
      />

      {blocked && blockedFull ? <BlockedModal key={botId} botId={botId} onClose={closeBlockedFull} /> : null}

      {}
      <div
        className="tab-panel"
        role="tabpanel"
        id={`${panelId}-panel`}
        aria-labelledby={`${panelId}-tab-${shellOpen ? 'shell' : tab === 'terminal' && !settingsOpen ? 'terminal' : tab === 'preview' && !settingsOpen ? 'preview' : 'chat'}`}
      >
      {shellOpen && shellView ? (
        <HostShellPanel key={`${shellView.host}:${shellView.paneId}`} host={shellView.host} paneId={shellView.paneId} cwd={shellView.cwd} embedded />
      ) : tab === 'preview' && !settingsOpen ? (
        <PreviewPanel key={botId} botId={botId} />
      ) : tab === 'terminal' && !settingsOpen ? (
        active ? (
          <TerminalTab botId={botId} />
        ) : (
          <EmptyState
            title={t('終端尚未就緒')}
            icon="▭"
            action={
              <button
                type="button"
                className="btn primary"
                disabled={Boolean(busy[`start:${botId}`]) || handedOff}
                onClick={() => void startBot(botId)}
              >
                {t('啟動 {name}', { name: bot.name })}
              </button>
            }
          >
            {t('Bot 未在執行中，沒有可讀取的終端。請先啟動 Bot。')}
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
          <HostOfflineChatNotice botId={botId} />
          {costOverviewOpen && apiBot ? (
            <ApiBotCostOverview
              botId={botId}
              onClose={() => setCostOverviewOpen(false)}
              onJumpToTurn={jumpToTurn}
            />
          ) : null}
          <MessageList botId={botId} />
          {!active && !waitingForStart && !isApiKind(bot.kind) ? (
            <div className="bot-stopped-bar" role="status">
              <span>{composerReason || t('Bot 未在執行中，無法送出訊息')}</span>
              <button
                type="button"
                className="mini-btn primary"
                disabled={Boolean(busy[`start:${botId}`]) || handedOff}
                title={t('啟動 {name}', { name: bot.name })}
                onClick={() => void startBot(botId)}
              >
                {t('啟動')}
              </button>
            </div>
          ) : null}
          <BackgroundJobsBar botId={botId} />
          {}
          <div className="chat-dock" ref={dockRef}>
            <PhoneOutboxTray />
            {noAttach ? <div className="composer-cost" role="status" data-k="openai-no-attach">{t('OpenAI API bot 不支援附件')}</div> : null}
            <Composer botId={botId} inputRef={composerRef} hideLock={!active} forceFocus={chatEmpty && active} files={files} />
          </div>
          {settingsOpen ? <BotSettingsPanel key={botId} botId={botId} /> : null}
        </div>
      )}
      </div>
    </>
  )
}
