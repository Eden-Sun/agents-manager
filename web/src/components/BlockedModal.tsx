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
 * agent 進 `blocked` 時彈出整個 herdr 畫面：`BlockedPanel` 的 300px 截角看不到完整問題與選項。
 * 鍵盤直通會把 Esc 也送給 agent，所以關閉只走 ✕／點視窗外（頁尾有寫）。
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

  // handler 每次 render 都是新的；透過 ref 讀，listener 才不必重新註冊（同 Modal.tsx）。
  const onCloseRef = useRef(onClose)
  useEffect(() => {
    onCloseRef.current = onClose
  })

  useDialogFocus(true, rootRef, { initialFocus: () => termRef.current })

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null
      const tag = target?.tagName.toLowerCase()
      // 模態期間只有這個視窗裡的輸入控制項能留住鍵盤，否則字會跑進背後的聊天輸入框。
      const inside = Boolean(rootRef.current?.contains(target))
      const editing = inside && (tag === 'input' || tag === 'textarea' || tag === 'select')
      if (editing) return
      if (e.key === 'Tab') return
      // 焦點在視窗內按鈕上時 Enter／Space 要啟動那顆按鈕，而不是送進 pane（否則會選到 TUI 游標那項）。
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

        {/* 選單模式只有一條主捲軸，問題行釘在上緣：兩塊各自捲在手機上分不清（2026-09-12 第二輪回饋第 4 點）。 */}
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
          {/* 選單模式預設不長這一段（第二輪回饋第 2 點）。 */}
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
