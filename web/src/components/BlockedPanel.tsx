import { KEYPAD, usePaneKeys } from '../hooks/usePaneKeys'
import { useTerminalSnapshot } from '../hooks/useTerminalSnapshot'
import { CodexUpdateHint } from './CodexUpdateHint'
import { linkifyTerm } from './TermLinks'
import { useTermWrap } from './termWrap'

/**
 * SPEC §3.2：agent `blocked` 時對話上方的終端快照 + 按鍵面板。
 *
 * 完整畫面在 `BlockedModal`（blocked 一發生就自動彈出來）。這條面板是它關掉之後的留守：
 * 狀態還在、隨時可以「展開全畫面」再叫回來。全畫面開著的時候 `paused` 會停掉這裡的輪詢，
 * 同一個 bot 不會有兩條 `GET /terminal` 在跑。
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
  // `pre` 是 white-space: pre，內容一律當成一個字串算好再放進去，免得 JSX 的排版縮排跑進畫面。
  const body = err
    ? `讀取終端失敗：${err}`
    : (snap?.text ?? (paused ? '（全畫面終端開著，這裡暫停更新）' : '讀取中…'))

  return (
    <section className="blocked" aria-label="終端等待回應">
      <div className="blocked-head">
        <span className="blocked-title">● agent 需要回應</span>
        <span className="blocked-sub">
          {paused ? (
            '全畫面開著，畫面在上面那個視窗'
          ) : (
            /* `visible` 是抓法、`revision` 是對帳用的序號：兩個都收進 tooltip。
               條上留下的是「這是即時的嗎」與「有沒有被截斷」。 */
            <span
              title={`終端 visible 快照${
                snap?.revision !== null && snap?.revision !== undefined ? `・revision ${snap.revision}` : ''
              }`}
            >
              終端畫面，每秒更新
              {snap?.truncated ? '・已截斷' : ''}
            </span>
          )}
        </span>
        {onExpand ? (
          <button type="button" className="mini-btn" title="展開整個 herdr 畫面" onClick={onExpand}>
            展開全畫面
          </button>
        ) : null}
      </div>
      {/* codex 的升級提示（TUI 當場問的）也要在這裡就能先看 changelog，不必先展開全畫面。 */}
      <CodexUpdateHint botId={botId} text={snap?.text} onAnswered={refresh} />
      <pre className={`term blocked-term${wrap ? ' term-wrap' : ''}`}>{linkifyTerm(body, snap?.columns)}</pre>
      <div className="keypad">
        {KEYPAD.map((k) => (
          <button key={k.label} type="button" className="key-btn" title={k.title} onClick={() => press(k.keys)}>
            {k.label}
          </button>
        ))}
        {/* 本來寫的是 `按鍵會帶 expect_run_id，Run 不符時後端回 409`——那是 API 的約定，
            不是使用者要知道的事。他要知道的是這顆按下去安不安全。 */}
        <span className="hint">按鍵只會送到目前這個 Run；bot 中途重啟就不會誤送。</span>
      </div>
    </section>
  )
}
