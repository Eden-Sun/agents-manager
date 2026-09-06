import { KEYPAD, usePaneKeys } from '../hooks/usePaneKeys'
import { useTerminalSnapshot } from '../hooks/useTerminalSnapshot'

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
            <>
              終端 <code>visible</code> 快照，每秒更新
              {snap?.revision !== null && snap?.revision !== undefined ? `（revision ${snap.revision}）` : ''}
              {snap?.truncated ? '・已截斷' : ''}
            </>
          )}
        </span>
        {onExpand ? (
          <button type="button" className="mini-btn" title="展開整個 herdr 畫面" onClick={onExpand}>
            展開全畫面
          </button>
        ) : null}
      </div>
      <pre className="term blocked-term">{body}</pre>
      <div className="keypad">
        {KEYPAD.map((k) => (
          <button key={k.label} type="button" className="key-btn" title={k.title} onClick={() => press(k.keys)}>
            {k.label}
          </button>
        ))}
        <span className="hint">按鍵會帶 expect_run_id，Run 不符時後端回 409</span>
      </div>
    </section>
  )
}
