import { useT } from "../lib/i18n"
import { useCodexTerminalFooter } from '../hooks/useCodexTerminalFooter'
import { useRef, useState } from 'react'
import { useChoiceMenu } from '../hooks/useChoiceMenu'
import { usePendingQuestion } from '../hooks/usePendingQuestion'
import { pendingAtOnScreen, questionVisibleOnScreen } from '../lib/pendingQuestion'
import { PendingQuestionCard } from './PendingQuestionCard'
import { KEYPAD, usePaneKeys } from '../hooks/usePaneKeys'
import { useTerminalSnapshot } from '../hooks/useTerminalSnapshot'
import { useStickToBottom } from '../hooks/useStickToBottom'
import { BlockedChoices, BlockedExtrasBar } from './BlockedChoices'
import { isSurvey } from '../lib/choiceDraft'
import { surveyDraftAllowed } from '../store/mobilePreview'
import { BlockedDraft } from './BlockedDraft'
import { CodexUpdateHint } from './CodexUpdateHint'
import { CodexAsyncQuestion } from './CodexAsyncQuestion'
import { parseAsyncScreen } from '../lib/codexAsyncQuestion'
import { isCodexUpdateMenu } from '../lib/codexUpdatePrompt'
import { linkifyTerm } from './termLinks'
import { useTermWrap } from './termWrap'
import { useStore } from '../store/store'
import './blockedPanel.css'

export function BlockedPanel({
  botId,
  paused = false,
  onExpand,
}: {
  botId: string
  paused?: boolean
  onExpand?: () => void
}) {
  const t = useT()
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind)
  const { snap, err, refresh } = useTerminalSnapshot(botId, { source: 'visible', lines: 40, paused })
  const terminal = useCodexTerminalFooter(botId, snap)
  const press = usePaneKeys(botId, refresh)
  const wrap = useTermWrap()
  const menu = useChoiceMenu(botId, snap?.text)
  const { questions: pending, status: pendingStatus } = usePendingQuestion(botId, paused)
  const showPending = pending.length > 0 && !questionVisibleOnScreen(pending, menu?.question)
  const pendingAt = pendingAtOnScreen(pending, menu)
  const [extras, setExtras] = useState(false)
  const body = err
    ? t('讀取終端失敗：{err}', { err })
    : (snap ? terminal.body : (paused ? t('（全畫面終端開著，這裡暫停更新）') : t('讀取中…')))
  const dedicatedOptions = kind === 'codex' && (
    isCodexUpdateMenu(snap?.text) || (
      pendingStatus === 'ready' && parseAsyncScreen(snap?.text)?.expanded === false &&
      pending.some((question) => question.options.length > 0)
    )
  )
  const showTerm = (!menu && !dedicatedOptions) || extras
  const showKeys = !menu || extras
  const termRef = useRef<HTMLPreElement>(null)
  const { onScroll: onTermScroll } = useStickToBottom(termRef, snap?.text, showTerm)

  return (
    <section className="blocked" aria-label={t("終端等待回應")}>
      <div className="blocked-head">
        <span
          className="blocked-title"
          title={snap?.revision != null ? t('終端 visible 快照・revision {rev}', { rev: snap.revision }) : t('終端 visible 快照')}
        >
          {t('● agent 需要回應')}
        </span>
        {menu ? null : (
          <span className="blocked-sub">
            {paused ? (
              t('全畫面開著，畫面在上面那個視窗')
            ) : (
              <span>
                {snap?.truncated ? t('終端畫面，每秒更新・已截斷') : t('終端畫面，每秒更新')}
              </span>
            )}
          </span>
        )}
        {onExpand ? (
          <button type="button" className="mini-btn" title={t("展開整個 herdr 畫面")} onClick={onExpand}>
            {t('展開全畫面')}
          </button>
        ) : null}
      </div>
      <CodexUpdateHint botId={botId} text={snap?.text} onAnswered={refresh} />
      <CodexAsyncQuestion botId={botId} text={snap?.text} questions={pending} onAnswered={refresh} />
      {pendingStatus === 'unavailable' ? <div role="status">{t("暫時無法確認待答題目，請參考終端畫面。")}</div> : null}
      {}
      {showPending && !menu ? <PendingQuestionCard questions={pending} defaultOpen answerWhere="below" /> : null}
      {}
      {menu && !paused ? (
        <>
          {surveyDraftAllowed(isSurvey(menu)) ? (
            <BlockedDraft botId={botId} menu={menu} onAnswered={refresh} />
          ) : (
            <BlockedChoices
              botId={botId}
              menu={menu}
              onAnswered={refresh}
              pendingAt={pendingAt}
            />
          )}
          {showPending ? <PendingQuestionCard questions={pending} current={pendingAt?.at ?? -1} /> : null}
          <BlockedExtrasBar botId={botId} open={extras} onToggle={() => setExtras((v) => !v)} onAnswered={refresh} />
        </>
      ) : null}
      {!menu && dedicatedOptions ? (
        <button type="button" className="mini-btn bc-more" aria-expanded={extras} onClick={() => setExtras((v) => !v)}>
          {extras ? t('收起終端原文') : t('終端原文與更多按鍵')}
        </button>
      ) : null}
      {showTerm ? (
        <pre ref={termRef} onScroll={onTermScroll} className={`term blocked-term${wrap ? ' term-wrap' : ''}`}>{linkifyTerm(body, snap?.columns)}</pre>
      ) : null}
      {showKeys ? (
        <div className="keypad">
          {KEYPAD.map((k) => (
            <button key={k.label} type="button" className="key-btn" title={t.tr(k.title)} onClick={() => press(k.keys)}>
              {k.label}
            </button>
          ))}
          <span className="hint">{t("按鍵只會送到目前這個 Run；bot 中途重啟就不會誤送。")}</span>
        </div>
      ) : null}
    </section>
  )
}
