import { useT } from '../lib/i18n'
import { useCodexTerminalFooter } from '../hooks/useCodexTerminalFooter'
import { useEffect, useRef, useState, type CSSProperties } from 'react'
import { createPortal } from 'react-dom'
import { useChoiceMenu } from '../hooks/useChoiceMenu'
import { usePendingQuestion } from '../hooks/usePendingQuestion'
import { pendingAtOnScreen, questionVisibleOnScreen } from '../lib/pendingQuestion'
import { PendingQuestionCard } from './PendingQuestionCard'
import { herdrKeyFromEvent, KEYPAD, usePaneKeys } from '../hooks/usePaneKeys'
import { blockedKeyAction, passthroughLive } from '../lib/blockedKeys'
import { isDangerousRmScreen } from '../lib/dangerousRm'
import { fitRules } from '../lib/termRules'
import { reflowForPhone } from '../lib/termReflow'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useTerminalSnapshot } from '../hooks/useTerminalSnapshot'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { useStickToBottom } from '../hooks/useStickToBottom'
import { useStore } from '../store/store'
import { BlockedChoices, BlockedExtrasBar } from './BlockedChoices'
import { isSurvey } from '../lib/choiceDraft'
import { surveyDraftAllowed } from '../store/mobilePreview'
import { BlockedDraft } from './BlockedDraft'
import { CodexUpdateHint } from './CodexUpdateHint'
import { CodexAsyncQuestion } from './CodexAsyncQuestion'
import { parseAsyncScreen } from '../lib/codexAsyncQuestion'
import { isCodexUpdateMenu } from '../lib/codexUpdatePrompt'

const READABLE_COLUMNS = 60
const WIDE_COLUMNS = 140

export function BlockedModal({
 botId, onClose }: { botId: string; onClose: () => void }) {
  const t = useT()
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const [passthrough, setPassthrough] = useState(false)
  const { snap, err, refresh } = useTerminalSnapshot(botId, { source: 'visible', lines: 200 })
  const terminal = useCodexTerminalFooter(botId, snap)
  const press = usePaneKeys(botId, refresh)
  const menu = useChoiceMenu(botId, snap?.text)
  const dangerous = isDangerousRmScreen(snap?.text)
  const keysLive = passthroughLive(passthrough, dangerous)
  const { questions: pending, status: pendingStatus } = usePendingQuestion(botId, false)
  const showPending = pending.length > 0 && !questionVisibleOnScreen(pending, menu?.question)
  const pendingAt = pendingAtOnScreen(pending, menu)
  const [extras, setExtras] = useState(false)
  const dedicatedOptions = bot?.kind === 'codex' && (
    isCodexUpdateMenu(snap?.text) || (
      pendingStatus === 'ready' && parseAsyncScreen(snap?.text)?.expanded === false &&
      pending.some((question) => question.options.length > 0)
    )
  )
  const showRaw = (!menu && !dedicatedOptions) || extras
  const rootRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<HTMLPreElement>(null)
  const phone = useMediaQuery(PHONE_QUERY)
  const [rawLayout, setRawLayout] = useState(false)
  const reflow = phone && !rawLayout
  const fit = !phone && snap?.columns != null && !err
  const { onScroll: onTermScroll } = useStickToBottom(termRef, snap?.text, phone && showRaw)

  const onCloseRef = useRef(onClose)
  const dangerousRef = useRef(dangerous)
  useEffect(() => {
    onCloseRef.current = onClose
    dangerousRef.current = dangerous
  })

  useDialogFocus(true, rootRef, { initialFocus: () => termRef.current })

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null
      const action = blockedKeyAction({
        passthrough,
        dangerous: dangerousRef.current,
        inside: Boolean(rootRef.current?.contains(target)),
        tag: target?.tagName.toLowerCase() ?? null,
        key: e.key,
        defaultPrevented: e.defaultPrevented,
      })
      if (action === 'browser') return
      if (action === 'close') {
        e.preventDefault()
        onCloseRef.current()
        return
      }
      const key = herdrKeyFromEvent(e)
      if (!key) return
      e.preventDefault()
      e.stopPropagation()
      press([key])
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [passthrough, press])

  const narrow = snap?.columns != null && snap.columns < READABLE_COLUMNS

  return createPortal(
    <div className="modal-backdrop" role="presentation" onMouseDown={onClose}>
      <div
        ref={rootRef}
        className={`modal blocked-modal${fit && showRaw && (snap?.columns ?? 0) > WIDE_COLUMNS ? ' is-wide' : ''}${showRaw ? '' : ' is-menu'}`}
        role="dialog"
        aria-modal="true"
        aria-label={t('{name} 需要回應', { name: bot?.name ?? 'agent' })}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <strong
            className="blocked-title"
            title={t('終端 visible 全畫面') + `${snap?.pane_id ? `・pane ${snap.pane_id}` : ''}${

              snap?.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''
            }${snap?.revision != null ? `・revision ${snap.revision}` : ''}`}
          >
            ● {t('{name} 需要回應', { name: bot?.name ?? 'agent' })}
          </strong>
          {}
          {menu ? null : (
            <span className="modal-sub">
              {t('終端畫面，每秒更新')}
              {snap?.pane_id ? `・pane ${snap.pane_id}` : ''}
              {snap?.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
            </span>
          )}
          {phone && showRaw ? (
            <button
              type="button"
              className="btn blocked-modal-layout"
              aria-pressed={rawLayout}
              title={rawLayout ? t('現在是終端原樣；按一下改回手機易讀排版') : t('現在是手機易讀排版（去掉外框、對齊用的空白與空列）；按一下看終端原樣')}
              onClick={() => setRawLayout((v) => !v)}
            >
              {t("原樣")}
            </button>
          ) : null}
          <button type="button" className="icon-btn" aria-label={t("關閉")} title={t("關閉")} onClick={onClose}>
            ✕
          </button>
        </div>

        {narrow && !menu ? (
          <div className="blocked-modal-warn" role="status">
            {t('這個 pane 只有 {cols} 欄，agent 的輸出在終端就被折成碎片了。到「終端」分頁可以把它移到自己的分頁，之後的輸出才會是完整寬度。', { cols: snap?.columns ?? 0 })}
          </div>
        ) : null}

        <CodexUpdateHint botId={botId} text={snap?.text} onAnswered={refresh} />
        <CodexAsyncQuestion botId={botId} text={snap?.text} questions={pending} onAnswered={refresh} />

        {pendingStatus === 'unavailable' ? <div role="status">{t("暫時無法確認待答題目，請參考終端畫面。")}</div> : null}
        {}
        {showPending && !menu ? <PendingQuestionCard questions={pending} defaultOpen answerWhere="below" /> : null}

        {}
        {menu ? (
          <div className="blocked-modal-body">
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
          </div>
        ) : null}

        {showRaw ? (
          <pre
            className={`term blocked-modal-term${reflow ? ' term-wrap is-reflow' : fit ? ' is-fit' : ' term-wrap'}`}
            style={fit ? ({ '--term-cols': snap?.columns } as CSSProperties) : undefined}
            ref={termRef}
            tabIndex={0}
            onScroll={onTermScroll}
          >
            {err ? t('讀取終端失敗：{err}', { err }) : snap?.text ? (reflow ? reflowForPhone(fitRules(terminal.body)) : fitRules(terminal.body)) : t('讀取中…')}
          </pre>
        ) : null}

        <div className="blocked-modal-foot">
          {!menu && dedicatedOptions ? (
            <button type="button" className="mini-btn bc-more" aria-expanded={extras} onClick={() => setExtras((v) => !v)}>
              {extras ? t('收起終端原文') : t('終端原文與更多按鍵')}
            </button>
          ) : null}
          {menu ? (
            <BlockedExtrasBar botId={botId} open={extras} onToggle={() => setExtras((v) => !v)} onAnswered={refresh} />
          ) : null}
          {}
          {showRaw ? (
            <div className="keypad">
              {KEYPAD.map((k) => (
                <button key={k.label} type="button" className="key-btn" title={t.tr(k.title)} onClick={() => press(k.keys)}>
                  {k.label}
                </button>
              ))}
            </div>
          ) : null}
          <div className="blocked-modal-hints">
            <label className={`conn${dangerous ? ' is-locked' : ''}`}>
              <input
                type="checkbox"
                checked={keysLive}
                disabled={dangerous}
                onChange={(e) => setPassthrough(e.target.checked)}
              />
              {t('鍵盤直通')}
            </label>
            {}
            <span className={`hint${!dangerous && !keysLive ? ' hint-keys-off' : ''}`}>
              {dangerous
                ? t('claude 的防誤刪框：只有你本人能核准，而且要按上面那兩個選項。這種框上不開放鍵盤直通——打字打到一個 1 就等於按下「1. Yes」。')
                : keysLive
                  ? t('打字、方向鍵、Enter、Esc、ctrl+c 都直接送進終端；Esc 也算，關閉請按右上 ✕ 或點視窗外。⌘ 快捷鍵（⌘C 複製）留給瀏覽器，Home / End / PgUp / PgDn 用來捲這個畫面。')
                  : t('鍵盤還給瀏覽器：Esc 關閉這個視窗，回應改用上面的選項或按鍵。要把整個鍵盤接進終端再打開這個開關。')}
            </span>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  )
}
