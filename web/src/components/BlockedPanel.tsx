import { useState } from 'react'
import { useChoiceMenu } from '../hooks/useChoiceMenu'
import { usePendingQuestion } from '../hooks/usePendingQuestion'
import { questionVisibleOnScreen } from '../lib/pendingQuestion'
import { PendingQuestionCard } from './PendingQuestionCard'
import { KEYPAD, usePaneKeys } from '../hooks/usePaneKeys'
import { useTerminalSnapshot } from '../hooks/useTerminalSnapshot'
import { BlockedChoices, BlockedExtrasBar } from './BlockedChoices'
import { isSurvey } from '../lib/choiceDraft'
import { surveyDraftAllowed } from '../store/mobilePreview'
import { BlockedDraft } from './BlockedDraft'
import { CodexUpdateHint } from './CodexUpdateHint'
import { linkifyTerm } from './termLinks'
import { useTermWrap } from './termWrap'
import './blockedPanel.css'

/**
 * SPEC §3.2：agent `blocked` 時對話上方的快照＋按鍵面板，`BlockedModal` 關掉後的留守；
 * 全畫面開著時 `paused` 停掉這裡的輪詢。認得出編號選單時走選單模式（2026-09-12 第二輪回饋）。
 */
export function BlockedPanel({
  botId,
  paused = false,
  onExpand,
}: {
  botId: string
  paused?: boolean
  onExpand?: () => void
}) {
  const { snap, err, refresh } = useTerminalSnapshot(botId, { source: 'visible', lines: 40, paused })
  const press = usePaneKeys(botId, refresh)
  // 折不折行跟終端分頁共用一個開關（切換鍵在那條 term-bar 上）。
  const wrap = useTermWrap()
  const menu = useChoiceMenu(botId, snap?.text)
  // 畫面上看不到題目（pane 太矮、claude 把選單裁掉）時，從 transcript 補題目（2026-09-23 使用者）。
  const pending = usePendingQuestion(botId, paused)
  const showPending = pending.length > 0 && !questionVisibleOnScreen(pending, menu?.question)
  const [extras, setExtras] = useState(false)
  // `pre` 是 white-space: pre，內容一律當成一個字串算好再放進去，免得 JSX 的排版縮排跑進畫面。
  const body = err
    ? `讀取終端失敗：${err}`
    : (snap?.text ?? (paused ? '（全畫面終端開著，這裡暫停更新）' : '讀取中…'))
  /** 選單模式預設把這兩樣收起來；認不出選單時它們就是這塊面板本身。 */
  const showTerm = !menu || extras
  const showKeys = !menu || extras

  return (
    <section className="blocked" aria-label="終端等待回應">
      <div className="blocked-head">
        <span
          className="blocked-title"
          title={`終端 visible 快照${snap?.revision != null ? `・revision ${snap.revision}` : ''}`}
        >
          ● agent 需要回應
        </span>
        {menu ? null : (
          <span className="blocked-sub">
            {paused ? (
              '全畫面開著，畫面在上面那個視窗'
            ) : (
              <span>
                終端畫面，每秒更新
                {snap?.truncated ? '・已截斷' : ''}
              </span>
            )}
          </span>
        )}
        {onExpand ? (
          <button type="button" className="mini-btn" title="展開整個 herdr 畫面" onClick={onExpand}>
            展開全畫面
          </button>
        ) : null}
      </div>
      <CodexUpdateHint botId={botId} text={snap?.text} onAnswered={refresh} />
      {showPending ? <PendingQuestionCard questions={pending} /> : null}
      {menu ? (
        <>
          {surveyDraftAllowed(isSurvey(menu)) ? (
            <BlockedDraft botId={botId} menu={menu} onAnswered={refresh} />
          ) : (
            <BlockedChoices botId={botId} menu={menu} onAnswered={refresh} />
          )}
          <BlockedExtrasBar botId={botId} open={extras} onToggle={() => setExtras((v) => !v)} onAnswered={refresh} />
        </>
      ) : null}
      {showTerm ? (
        <pre className={`term blocked-term${wrap ? ' term-wrap' : ''}`}>{linkifyTerm(body, snap?.columns)}</pre>
      ) : null}
      {showKeys ? (
        <div className="keypad">
          {KEYPAD.map((k) => (
            <button key={k.label} type="button" className="key-btn" title={k.title} onClick={() => press(k.keys)}>
              {k.label}
            </button>
          ))}
          <span className="hint">按鍵只會送到目前這個 Run；bot 中途重啟就不會誤送。</span>
        </div>
      ) : null}
    </section>
  )
}
