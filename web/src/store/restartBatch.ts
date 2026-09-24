/**
 * 一鍵重啟（SPEC §6.9）的進度怎麼累加。拆出來是因為事件處理在 socket 裡，store 動作測不到它，
 * 而這段有兩個會算錯的邊界（daemon 902a997）：
 * - 輪到那一顆時狀態變了（開始回合、被派了工）：`status:"skipped"`，不重啟、但算處理過了；
 * - 按下去時已經有一批在跑：daemon 回那一批的 id、`total: 0`，總數要從事件補。
 */
import { num, pick, str } from '../api/normalize'
import type { RestartBatch } from '../api/types'

type Rec = Record<string, unknown>

/** 一則 `bots_restart_progress`。不是這一批的、看不懂的狀態都原樣回傳（呼叫端據此判斷沒變）。 */
export function restartProgress(b: RestartBatch, data: Rec): RestartBatch {
  if (str(pick(data, 'batch_id')) !== b.id) return b
  const name = str(pick(data, 'name'))
  const status = str(pick(data, 'status'))
  // 加入的是別人按出來的那一批時，一開始不知道總數。
  const total = Math.max(b.total, num(pick(data, 'total'), 0))
  const base = total !== b.total ? { ...b, total } : b
  switch (status) {
    case 'restarting':
      return { ...base, current: name }
    case 'ok':
      return { ...base, done: base.done + 1, current: null, ok: [...base.ok, name] }
    case 'failed':
      return {
        ...base,
        done: base.done + 1,
        current: null,
        failed: [...base.failed, { name, error: str(pick(data, 'error'), '失敗') }],
      }
    case 'skipped':
      return {
        ...base,
        done: base.done + 1,
        current: null,
        skipped: [
          ...base.skipped,
          {
            bot_id: str(pick(data, 'bot_id')),
            name,
            reason: str(pick(data, 'reason')),
            reason_label: str(pick(data, 'reason_label'), '狀態變了，這次不重啟'),
          },
        ],
      }
    default:
      return base
  }
}

/**
 * 快照說的「現在有沒有批次在跑」跟手上這一份對帳（issue #492）。
 *
 * 進度只走 WS，`bots_restart_done` 是唯一會把它收尾的來源，而它有兩條收不到的路：批次跑到一半
 * daemon 重啟（那一則永遠不會送），或客戶端落到全量 resync（`refreshState` 不重播 backlog）。
 * 收不到就會永遠停在「重啟中 k/N」，而且那顆晶片會一直蓋住一鍵重啟的觸發鈕。
 *
 * - `undefined`（舊 daemon 沒有這個欄位）＝**不知道**，不動手上的：清掉正在跑的進度比留著更糟。
 * - 同一批還在跑：原樣留著（連物件都不換，免得白重繪）。
 * - daemon 說在跑的是**另一批**：換成那一批（總數等事件補）。手上這份已經過期，而進度只走 WS、
 *   `restartProgress` 只收 id 對得上的事件——不換的話那一批的進度會被整段丟掉。跑完的摘要也一樣要換：
 *   摘要是給人看的，但不能因為它還沒被收起來，就讓後來那批重啟整個看不見。
 * - daemon 說沒有批次在跑：正在跑的那份已經過期，清掉（晶片跟著變回一鍵重啟）；已經 `finished` 的
 *   摘要留著，那是給人看的，要由使用者自己收起來。
 */
export function reconcileBatch(current: RestartBatch | null, daemonBatchId: string | null | undefined): RestartBatch | null {
  if (!current || daemonBatchId === undefined || daemonBatchId === current.id) return current
  if (daemonBatchId !== null) return joinRunningBatch(null, daemonBatchId)
  return current.finished ? current : null
}

/** `already_running` 的回應：已經在看同一批就不動，否則換成那一批（總數等事件補）。 */
export function joinRunningBatch(current: RestartBatch | null, batchId: string): RestartBatch {
  if (current && current.id === batchId) return current
  return { id: batchId, total: 0, done: 0, current: null, ok: [], failed: [], skipped: [], finished: false }
}

