import { useStore } from '../store/store'

/**
 * 「claude 有更新」按下去之後的那條進度／摘要（SPEC §6.9）。
 *
 * 觸發那顆按鈕原本也在這裡（側欄搜尋框上面一整條）。現在搬到額度列上的 chip
 * （`UpdateQuotaChip`）：那一列每個畫面都在，側欄收起來也看得到，而「有沒有新版」跟
 * 「還剩多少額度」本來就是同一個 kind 的全域狀態。
 *
 * 留在側欄的是**結果**：跑到第幾顆、哪幾顆失敗、哪幾顆被跳過與為什麼。這幾行放不進標題列的
 * chip 裡，而它們才是要動手的部分——chip 上只寫成功幾顆，名單看這裡。整條寬度、只在真的按過
 * 之後出現，收掉之後一個 pixel 都不佔。
 */
export function UpdateAllBanner() {
  const batch = useStore((s) => s.restartBatch)
  if (!batch) return null
  return <BatchRow />
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
