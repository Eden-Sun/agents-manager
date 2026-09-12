import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useChoiceMenu } from '../hooks/useChoiceMenu'
import { herdrKeyFromEvent, KEYPAD, usePaneKeys } from '../hooks/usePaneKeys'
import { useTerminalSnapshot } from '../hooks/useTerminalSnapshot'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { useStore } from '../store/store'
import { BlockedChoices, BlockedExtrasBar } from './BlockedChoices'
import { isSurvey } from '../lib/choiceDraft'
import { BlockedDraft } from './BlockedDraft'
import { CodexUpdateHint } from './CodexUpdateHint'

/** 低於這個欄數，TUI 會把自己的輸出折成碎片，畫面本身就讀不了（同 TerminalTab）。 */
const READABLE_COLUMNS = 60

/**
 * agent 進 `blocked` 時彈出來的**整個 herdr 畫面**。
 *
 * 對話上方那條 `BlockedPanel` 只放得下 300px 高的截角，而要決定「該按 y 還是 n」通常得看到
 * 完整的對話框：問題全文、選項、游標停在哪一個。所以 blocked 一發生就把整張畫面推到眼前，
 * 判斷跟回應在同一個地方完成。
 *
 * 鍵盤直通（預設開）把按鍵原樣送進 pane，等於直接在終端上操作。代價是 **Esc 也會送給
 * agent**，所以關閉只走 ✕ / 點視窗外；這件事直接寫在頁尾，不讓人按 Esc 按到疑惑。
 */
export function BlockedModal({ botId, onClose }: { botId: string; onClose: () => void }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const [passthrough, setPassthrough] = useState(true)
  const { snap, err, refresh } = useTerminalSnapshot(botId, { source: 'visible', lines: 200 })
  const press = usePaneKeys(botId, refresh)
  const menu = useChoiceMenu(botId, snap?.text)
  /** 選單模式預設只留問題與選項；終端原文、整排按鍵與鍵盤直通收在這顆開關後面。 */
  const [extras, setExtras] = useState(false)
  const showRaw = !menu || extras
  const rootRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<HTMLPreElement>(null)

  // Escape / 關閉鈕的 handler 每次 render 都是新的；透過 ref 讀，下面的 listener 才不必
  // 跟著重新註冊（同 Modal.tsx 的作法）。
  const onCloseRef = useRef(onClose)
  useEffect(() => {
    onCloseRef.current = onClose
  })

  // 焦點落在終端本身而不是第一顆按鈕；Tab 仍由共用 hook 留在這個視窗內。
  useDialogFocus(true, rootRef, { initialFocus: () => termRef.current })

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null
      const tag = target?.tagName.toLowerCase()
      // 只有這個視窗裡的輸入控制項（直通開關）能留住鍵盤；視窗外的輸入框在模態期間不該收到
      // 任何東西——沒有這個判斷，背後聊天輸入框有焦點時打的字會跑進去。
      const inside = Boolean(rootRef.current?.contains(target))
      const editing = inside && (tag === 'input' || tag === 'textarea' || tag === 'select')
      if (editing) return
      if (e.key === 'Tab') return
      // 焦點在這個視窗裡的按鈕（✕、選單模式的 `.bc-item`、按鍵列）上按 Enter／Space 要啟動那顆
      // 按鈕，不是把 enter／space 送進 pane——不然選單模式自動彈出、焦點落在 ✕ 時一按 Enter，
      // 被選走的是 TUI 游標所在那一項，鍵盤使用者也永遠用不到可點清單。
      if (inside && (tag === 'button' || tag === 'a') && (e.key === 'Enter' || e.key === ' ')) return

      if (!passthrough) {
        if (e.key === 'Escape' && !e.defaultPrevented) {
          e.preventDefault()
          onCloseRef.current()
        }
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
          {/* 抓法、pane id、幾欄幾列、對帳序號都是除錯資訊：選單模式下畫面上只留「要回答的
              那件事」，這些收進標題的 tooltip（2026-09-12 第二輪回饋第 3 點）。 */}
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

        {/* 選單模式：視窗裡**只有一條主捲軸**（這個 body），問題那一行釘在上緣。原本清單與終端
            快照各自捲，手指在手機上分不清正在捲哪一塊（2026-09-12 第二輪回饋第 4 點）。 */}
        {menu ? (
          <div className="blocked-modal-body">
            {isSurvey(menu) ? (
            <BlockedDraft botId={botId} menu={menu} onAnswered={refresh} />
          ) : (
            <BlockedChoices botId={botId} menu={menu} onAnswered={refresh} />
          )}
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
          {/* 選單模式預設不長這一段（第二輪回饋第 2 點）：那排鍵、鍵盤直通與它那段說明
              都不是回答問題需要的東西。展開之後樣子照舊。 */}
          {showRaw ? (
            <>
              <div className="keypad">
                {KEYPAD.map((k) => (
                  <button
                    key={k.label}
                    type="button"
                    className="key-btn"
                    title={k.title}
                    onClick={() => press(k.keys)}
                  >
                    {k.label}
                  </button>
                ))}
              </div>
              <div className="blocked-modal-hints">
                <label className="conn">
                  <input
                    type="checkbox"
                    checked={passthrough}
                    onChange={(e) => setPassthrough(e.target.checked)}
                  />
                  鍵盤直通
                </label>
                <span className="hint">
                  {passthrough
                    ? '打字、方向鍵、Enter、Esc、ctrl+c 都直接送進終端；Esc 也算，關閉請按右上 ✕ 或點視窗外。⌘ 快捷鍵（⌘C 複製）留給瀏覽器，Home / End / PgUp / PgDn 用來捲這個畫面。'
                    : '鍵盤還給瀏覽器：Esc 關閉這個視窗，回應改用上面的按鍵。'}
                </span>
              </div>
            </>
          ) : null}
        </div>
      </div>
    </div>,
    document.body,
  )
}
