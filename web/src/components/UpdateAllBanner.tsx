import { inFlightTurn, useStore } from '../store/store'
import { updateBatchCounts } from '../lib/updateBatch'

/**
 * 「claude 有更新 · 重啟 N 顆閒置的 Bot」（SPEC §6.9）。
 *
 * claude 下載好新版之後只會在每顆 bot 的 pane 底下印一句 `Update installed · Restart to
 * update`，套用的方式就是重啟。十顆 bot 就是點十次「重啟」，而且每點一次都要自己先確認那顆
 * 有沒有在忙——這條橫幅把它變成一顆按鈕：閒置的一次全部 exit + `--resume` 接回來，忙的跳過
 * 並把是哪幾顆、為什麼寫在下面。
 *
 * 位置在側欄標題底下、搜尋框上面：**全域**的動作放在標題列那排徽章旁邊會擠掉本來就很滿的
 * 一行，放在對話標題列又會變成「這顆 bot 的事」。這裡整條寬度、只在真的有更新時出現，做完
 * 收掉，平常完全不佔位。
 *
 * 進度與摘要都畫在同一條上，不另外開面板：這件事從按下去到看完結果只有一個節奏，中途切走
 * 反而找不回來。
 */
export function UpdateAllBanner() {
  const batch = useStore((s) => s.restartBatch)
  const restartIdleBots = useStore((s) => s.restartIdleBots)
  const sending = useStore((s) => Boolean(s.busy['restart-idle']))
  // 前端只負責畫按鈕的數字；真正動哪幾顆由 daemon 在按下去那一刻算（lib/updateBatch.ts）。
  //
  // 兩個 selector 各自回**純值**，不回物件或陣列：`updateBatchCounts` 每次都是新的陣列，
  // 包成一個物件回去的話連 `useShallow` 都擋不住（它比的是第一層，陣列還是新的），
  // React 會噴 `getSnapshot should be cached` 並把整條側欄打掉——側欄別處踩過同一個坑。
  const readyCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.length)
  const busyLines = useStore((s) =>
    updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
      .busy.map((b) => `${b.name}（${b.why}）`)
      .join('\n'),
  )
  const busyCount = busyLines ? busyLines.split('\n').length : 0

  if (batch) return <BatchRow />
  if (readyCount === 0 && busyCount === 0) return null

  return (
    <div className="update-all">
      <button
        type="button"
        className="update-all-go"
        disabled={sending || readyCount === 0}
        title={
          readyCount === 0
            ? '有更新等著套用，但這些 Bot 現在都在忙——等它們停下來再按'
            : '結束目前的 agent，再用同一個 session --resume 接回來（上下文不會掉）'
        }
        onClick={() => void restartIdleBots()}
      >
        {sending
          ? '整理中…'
          : readyCount === 0
            ? 'claude 有更新 · 目前沒有閒置的 Bot'
            : `claude 有更新 · 重啟 ${readyCount} 顆閒置的 Bot`}
      </button>
      {busyCount > 0 ? (
        <p className="update-all-note" title={busyLines}>
          {busyCount} 顆在忙，會先跳過
        </p>
      ) : null}
    </div>
  )
}

/** 送出之後的那一條：跑的時候是進度，跑完是摘要。 */
function BatchRow() {
  const batch = useStore((s) => s.restartBatch)
  const clear = useStore((s) => s.clearRestartBatch)
  if (!batch) return null

  const { total, done, current, ok, failed, skipped, finished } = batch
  const pct = total > 0 ? Math.round((done / total) * 100) : 100

  return (
    <div className={`update-all running${finished ? ' done' : ''}`}>
      <div className="update-all-line">
        <strong>
          {finished
            ? `重啟完成 · 成功 ${ok.length} 顆${skipped.length ? ` · 跳過 ${skipped.length} 顆` : ''}${
                failed.length ? ` · 失敗 ${failed.length} 顆` : ''
              }`
            : `重啟中 ${done}/${total}${current ? ` · ${current}` : ''}`}
        </strong>
        {finished ? (
          <button type="button" className="update-all-x" aria-label="收起這則摘要" onClick={clear}>
            ✕
          </button>
        ) : null}
      </div>
      {finished ? null : (
        <div className="update-all-bar" role="progressbar" aria-valuenow={done} aria-valuemin={0} aria-valuemax={total}>
          <span style={{ width: `${pct}%` }} />
        </div>
      )}
      {/* 失敗的先列：那是要動手的。跳過的接在後面，理由用 daemon 給的那句。 */}
      {failed.length > 0 ? (
        <ul className="update-all-list bad">
          {failed.map((f) => (
            <li key={f.name}>
              <b>{f.name}</b>
              {f.error}
            </li>
          ))}
        </ul>
      ) : null}
      {finished && skipped.length > 0 ? (
        <ul className="update-all-list">
          {skipped.map((sk) => (
            <li key={sk.bot_id}>
              <b>{sk.name}</b>
              {sk.reason_label}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}
