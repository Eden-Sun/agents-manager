import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useChoiceMenu } from '../hooks/useChoiceMenu'
import { usePendingQuestion } from '../hooks/usePendingQuestion'
import { pendingAtOnScreen, questionVisibleOnScreen } from '../lib/pendingQuestion'
import { PendingQuestionCard } from './PendingQuestionCard'
import { herdrKeyFromEvent, KEYPAD, usePaneKeys } from '../hooks/usePaneKeys'
import { blockedKeyAction, passthroughLive } from '../lib/blockedKeys'
import { isDangerousRmScreen } from '../lib/dangerousRm'
import { useTerminalSnapshot } from '../hooks/useTerminalSnapshot'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { useStore } from '../store/store'
import { BlockedChoices, BlockedExtrasBar } from './BlockedChoices'
import { isSurvey } from '../lib/choiceDraft'
import { surveyDraftAllowed } from '../store/mobilePreview'
import { BlockedDraft } from './BlockedDraft'
import { CodexUpdateHint } from './CodexUpdateHint'

/** 低於這個欄數，TUI 會把自己的輸出折成碎片，畫面本身就讀不了（同 TerminalTab）。 */
const READABLE_COLUMNS = 60

/**
 * agent 進 `blocked` 時彈出整個 herdr 畫面：`BlockedPanel` 的 300px 截角看不到完整問題與選項。
 *
 * **鍵盤直通預設關**（issue #545）：這個視窗是 `blocked` 後自己彈出來的，使用者沒有要求它；
 * 直通開著時打字會一個字一個字送進 TUI，在 `1. Yes / 2. No` 這種框上按到一個 `1` 就等於核准了。
 * 打開之後 Esc 也會送給 agent，所以那時關閉只走 ✕／點視窗外（開關旁邊有寫）。
 * #423 的防誤刪框一律鎖死直通：那一下只有使用者本人能按，而且要按在按鈕上。
 */
export function BlockedModal({ botId, onClose }: { botId: string; onClose: () => void }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const [passthrough, setPassthrough] = useState(false)
  const { snap, err, refresh } = useTerminalSnapshot(botId, { source: 'visible', lines: 200 })
  const press = usePaneKeys(botId, refresh)
  const menu = useChoiceMenu(botId, snap?.text)
  /** #423 的防誤刪框：直通鎖死、開關不給開（SPEC §3.2「只有使用者本人能核准」）。 */
  const dangerous = isDangerousRmScreen(snap?.text)
  const keysLive = passthroughLive(passthrough, dangerous)
  // 畫面上看不到題目（pane 太矮、claude 把選單裁掉）時，從 transcript 補題目（2026-09-23 使用者）。
  const pending = usePendingQuestion(botId, false)
  const showPending = pending.length > 0 && !questionVisibleOnScreen(pending, menu?.question)
  // 畫面上是原題的第幾題：補題目、補「第幾題／上一題下一題」（issue #559）。
  const pendingAt = pendingAtOnScreen(pending, menu)
  /** 選單模式預設只留問題與選項；終端原文、整排按鍵與鍵盤直通收在這顆開關後面。 */
  const [extras, setExtras] = useState(false)
  const showRaw = !menu || extras
  const rootRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<HTMLPreElement>(null)

  // handler 每次 render 都是新的；透過 ref 讀，listener 才不必重新註冊（同 Modal.tsx）。
  const onCloseRef = useRef(onClose)
  // 每秒一張新快照：`dangerous` 走 ref，listener 才不必跟著重掛（同 `onClose`）。
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
        // 模態期間只有這個視窗裡的輸入控制項能留住鍵盤，否則字會跑進背後的聊天輸入框。
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
        className="modal blocked-modal"
        role="dialog"
        aria-modal="true"
        aria-label={`${bot?.name ?? 'agent'} 需要回應`}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <strong
            className="blocked-title"
            title={`終端 visible 全畫面${snap?.pane_id ? `・pane ${snap.pane_id}` : ''}${
              snap?.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''
            }${snap?.revision != null ? `・revision ${snap.revision}` : ''}`}
          >
            ● {bot?.name ?? 'agent'} 需要回應
          </strong>
          {/* 除錯資訊選單模式下收進標題 tooltip（2026-09-12 第二輪回饋第 3 點）。 */}
          {menu ? null : (
            <span className="modal-sub">
              終端畫面，每秒更新
              {snap?.pane_id ? `・pane ${snap.pane_id}` : ''}
              {snap?.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
            </span>
          )}
          <button type="button" className="icon-btn" aria-label="關閉" title="關閉" onClick={onClose}>
            ✕
          </button>
        </div>

        {narrow && !menu ? (
          <div className="blocked-modal-warn" role="status">
            這個 pane 只有 {snap?.columns} 欄，agent 的輸出在終端就被折成碎片了。到「終端」分頁可以把它
            移到自己的分頁，之後的輸出才會是完整寬度。
          </div>
        ) : null}

        <CodexUpdateHint botId={botId} text={snap?.text} onAnswered={refresh} />

        {/* 認不出選單時原題攤開、排在終端上面（那時它是唯一讀得懂的題目）；認得出時收進下面的主捲軸（#559）。 */}
        {showPending && !menu ? <PendingQuestionCard questions={pending} defaultOpen answerWhere="below" /> : null}

        {/* 選單模式只有一條主捲軸，問題行釘在上緣：兩塊各自捲在手機上分不清（2026-09-12 第二輪回饋第 4 點）。
            原題收成一行排在選項**後面**：手機第一屏要先給能點的選項（issue #559）。 */}
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
          <pre className="term blocked-modal-term" ref={termRef} tabIndex={0}>
            {err ? `讀取終端失敗：${err}` : (snap?.text ?? '讀取中…')}
          </pre>
        ) : null}

        <div className="blocked-modal-foot">
          {menu ? (
            <BlockedExtrasBar botId={botId} open={extras} onToggle={() => setExtras((v) => !v)} onAnswered={refresh} />
          ) : null}
          {/* 整排按鍵在選單模式收起來（第二輪回饋第 2 點）；鍵盤直通那條**每個模式都要看得見**
              （#545：選單模式看不到它開著，也關不掉）。 */}
          {showRaw ? (
            <div className="keypad">
              {KEYPAD.map((k) => (
                <button key={k.label} type="button" className="key-btn" title={k.title} onClick={() => press(k.keys)}>
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
              鍵盤直通
            </label>
            {/* 直通關著時那段在講實體鍵盤：手機上藏起來，把高度留給選項（#559）；開關本身照樣看得見（#545）。 */}
            <span className={`hint${!dangerous && !keysLive ? ' hint-keys-off' : ''}`}>
              {dangerous
                ? 'claude 的防誤刪框：只有你本人能核准，而且要按上面那兩個選項。這種框上不開放鍵盤直通——打字打到一個 1 就等於按下「1. Yes」。'
                : keysLive
                  ? '打字、方向鍵、Enter、Esc、ctrl+c 都直接送進終端；Esc 也算，關閉請按右上 ✕ 或點視窗外。⌘ 快捷鍵（⌘C 複製）留給瀏覽器，Home / End / PgUp / PgDn 用來捲這個畫面。'
                  : '鍵盤還給瀏覽器：Esc 關閉這個視窗，回應改用上面的選項或按鍵。要把整個鍵盤接進終端再打開這個開關。'}
            </span>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  )
}
