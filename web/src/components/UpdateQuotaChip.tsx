import { inFlightTurn, useStore } from '../store/store'
import { updateBatchCounts } from '../lib/updateBatch'

/**
 * 「claude 有更新」擺在額度列上（SPEC §6.9）。
 *
 * 原本是側欄搜尋框上面一整條橫幅。但那條只在側欄裡看得到——手機把側欄收起來、看 team 面板、
 * 或視窗窄到側欄讓位時，就完全沒有提示；而「claude 有沒有新版」跟「claude 還剩多少額度」是
 * 同一件事的兩面（都是這個 kind 的全域狀態，跟你現在選哪顆 bot 無關），額度列本來就已經
 * 掛在每個畫面的標題列上，且每個 kind 一格。所以更新提示搬進來，跟 kind 的量表排在一起。
 *
 * 條子上放不下「重啟 N 顆閒置的 Bot」那句話，所以 chip 只留箭頭與數字，整句話走 tooltip 與
 * `aria-label`（跟同一列的停用開關同一套做法）。按下去做的事完全沒變：閒置的一次全部
 * exit + `--resume` 接回來，忙的跳過。
 *
 * 跑起來之後 chip 自己顯示 `done/total`，做完顯示成功幾顆並且點一下收掉——側欄那條橫幅仍然
 * 畫完整的進度與失敗／跳過名單（`UpdateAllBanner`），這裡是隨處都看得見的那份精簡版。
 */
export function UpdateQuotaChip() {
  const batch = useStore((s) => s.restartBatch)
  const restartIdleBots = useStore((s) => s.restartIdleBots)
  const clear = useStore((s) => s.clearRestartBatch)
  const sending = useStore((s) => Boolean(s.busy['restart-idle']))
  // 兩個 selector 各自回**純值**：`updateBatchCounts` 每次都回新的陣列，包成物件回去連
  // `useShallow` 都擋不住，React 會噴 `getSnapshot should be cached`（側欄踩過同一個坑）。
  const readyCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.length)
  const readyNames = useStore((s) =>
    updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
      .ready.map((b) => b.name)
      .join('、'),
  )
  const busyLines = useStore((s) =>
    updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
      .busy.map((b) => `${b.name}（${b.why}）`)
      .join('\n'),
  )
  const busyCount = busyLines ? busyLines.split('\n').length : 0

  if (batch) {
    const { total, done, ok, failed, finished } = batch
    const summary = finished
      ? `重啟完成 · 成功 ${ok.length} 顆${failed.length ? ` · 失敗 ${failed.length} 顆` : ''}`
      : `重啟中 ${done}/${total}`
    return (
      <button
        type="button"
        className={`quota-update running${finished ? ' done' : ''}${failed.length ? ' bad' : ''}`}
        title={finished ? `${summary}（點一下收起，詳細名單在側欄）` : summary}
        aria-label={summary}
        disabled={!finished}
        onClick={finished ? clear : undefined}
      >
        <span aria-hidden="true">{finished ? (failed.length ? '⚠' : '✓') : '⬆'}</span>
        <span className="quota-update-n" aria-hidden="true">
          {finished ? ok.length : `${done}/${total}`}
        </span>
      </button>
    )
  }

  if (readyCount === 0 && busyCount === 0) return null

  const busyNote = busyCount > 0 ? `\n${busyCount} 顆在忙，會先跳過：\n${busyLines}` : ''
  const label =
    readyCount === 0
      ? 'claude 有更新，但這些 Bot 現在都在忙——等它們停下來再按'
      : `claude 有更新 · 重啟 ${readyCount} 顆閒置的 Bot（結束目前的 agent，再用同一個 session --resume 接回來，上下文不會掉）`

  return (
    <button
      type="button"
      className={`quota-update${readyCount === 0 ? ' waiting' : ''}`}
      disabled={sending || readyCount === 0}
      title={`${label}${readyCount > 0 ? `：\n${readyNames}` : ''}${busyNote}`}
      aria-label={label}
      onClick={() => void restartIdleBots()}
    >
      <span aria-hidden="true">⬆</span>
      <span className="quota-update-n" aria-hidden="true">
        {sending ? '…' : readyCount === 0 ? busyCount : readyCount}
      </span>
    </button>
  )
}
