import { num, pick, str } from '../api/normalize'
import type { RestartBatch } from '../api/types'

type Rec = Record<string, unknown>

export function restartProgress(b: RestartBatch, data: Rec): RestartBatch {
  if (str(pick(data, 'batch_id')) !== b.id) return b
  const name = str(pick(data, 'name'))
  const status = str(pick(data, 'status'))
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

export function reconcileBatch(current: RestartBatch | null, daemonBatchId: string | null | undefined): RestartBatch | null {
  if (!current || daemonBatchId === undefined || daemonBatchId === current.id) return current
  if (daemonBatchId !== null) return joinRunningBatch(null, daemonBatchId)
  return current.finished ? current : null
}

export function joinRunningBatch(current: RestartBatch | null, batchId: string): RestartBatch {
  if (current && current.id === batchId) return current
  return { id: batchId, total: 0, done: 0, current: null, ok: [], failed: [], skipped: [], finished: false }
}

