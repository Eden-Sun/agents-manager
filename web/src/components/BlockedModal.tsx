import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { herdrKeyFromEvent, KEYPAD, usePaneKeys } from '../hooks/usePaneKeys'
import { useTerminalSnapshot } from '../hooks/useTerminalSnapshot'
import { useStore } from '../store/store'

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
  const rootRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<HTMLPreElement>(null)

  // Escape / 關閉鈕的 handler 每次 render 都是新的；透過 ref 讀，下面的 listener 才不必
  // 跟著重新註冊（同 Modal.tsx 的作法）。
  const onCloseRef = useRef(onClose)
  useEffect(() => {
    onCloseRef.current = onClose
  })

  // 焦點落在終端本身而不是第一顆按鈕：這樣 PageUp / 滾輪可以捲畫面，而且不會有「空白鍵
  // 誤觸某個按鈕」這種事——空白鍵本來就該送進 agent。
  useEffect(() => {
    const raf = requestAnimationFrame(() => termRef.current?.focus())
    return () => cancelAnimationFrame(raf)
  }, [])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null
      const tag = target?.tagName.toLowerCase()
      // 只有這個視窗裡的輸入控制項（直通開關）能留住鍵盤；視窗外的輸入框在模態期間不該收到
      // 任何東西——沒有這個判斷，背後聊天輸入框有焦點時打的字會跑進去。
      const editing =
        rootRef.current?.contains(target) && (tag === 'input' || tag === 'textarea' || tag === 'select')
      if (editing) return

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
          <strong className="blocked-title">● {bot?.name ?? 'agent'} 需要回應</strong>
          <span className="modal-sub">
            終端 <code>visible</code> 全畫面，每秒更新
            {snap?.pane_id ? `・pane ${snap.pane_id}` : ''}
            {snap?.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
            {snap?.revision != null ? `・revision ${snap.revision}` : ''}
          </span>
          <button type="button" className="icon-btn" aria-label="關閉" title="關閉" onClick={onClose}>
            ✕
          </button>
        </div>

        {narrow ? (
          <div className="blocked-modal-warn" role="status">
            這個 pane 只有 {snap?.columns} 欄，agent 的輸出在終端就被折成碎片了。到「終端」分頁可以把它
            移到自己的分頁，之後的輸出才會是完整寬度。
          </div>
        ) : null}

        <pre className="term blocked-modal-term" ref={termRef} tabIndex={0}>
          {err ? `讀取終端失敗：${err}` : (snap?.text ?? '讀取中…')}
        </pre>

        <div className="blocked-modal-foot">
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
        </div>
      </div>
    </div>,
    document.body,
  )
}
