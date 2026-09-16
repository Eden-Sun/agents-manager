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

/** `already_running` 的回應：已經在看同一批就不動，否則換成那一批（總數等事件補）。 */
export function joinRunningBatch(current: RestartBatch | null, batchId: string): RestartBatch {
  if (current && current.id === batchId) return current
  return { id: batchId, total: 0, done: 0, current: null, ok: [], failed: [], skipped: [], finished: false }
}

