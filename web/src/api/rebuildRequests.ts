/**
 * 左上角「重建 N/門檻」那顆 chip 的資料（使用者 2026-09-14）。
 *
 * 唯讀，走既有的 `GET /api/supervisor/approvals`；計數規則在 `lib/rebuildCount.ts`，與
 * `scripts/ops/daemon-update-kick.sh` 同一套。端點不存在（舊 daemon）或壞掉時回 `null`，
 * chip 整顆不出現——它不是錯誤面板。
 */
import { rawTransport } from './index'
import { ApiError } from './types'
import { pendingRebuilds, type RebuildRequest } from '../lib/rebuildCount'

/** daemon 還沒有「門檻是多少」的欄位，所以前端也用 5（見 docs/UI-DECISIONS.md）。 */
export const REBUILD_THRESHOLD = 5

function toRow(v: unknown): RebuildRequest | null {
  if (typeof v !== 'object' || v === null) return null
  const o = v as Record<string, unknown>
  if (o.purpose !== 'rebuild') return null
  const s = (k: string): string => (typeof o[k] === 'string' ? (o[k] as string) : '')
  const id = s('id')
  if (!id) return null
  return { id, requester: s('requester'), target_commit: s('target_commit'), scope: s('scope'), status: s('status'), created_at: s('created_at') }
}

/** mock 的假資料：`VITE_MOCK=1` 下要看得到 chip 與清單長什麼樣，不然只能對真 daemon 看。 */
const MOCK_ROWS: RebuildRequest[] = [
  { id: 'm1', requester: 'agents-manager-k8bw2f', target_commit: '4d5fe88aa1', scope: 'codex 改 effort 不該重啟', status: 'pending', created_at: '2026-09-14T02:10:00.000Z' },
  { id: 'm2', requester: 'agents-manager-mkng2n', target_commit: '4d5fe88aa1', scope: '瘦身 C：DB migrate 只支援現行 schema', status: 'approved', created_at: '2026-09-14T01:40:00.000Z' },
  { id: 'm3', requester: 'ag-man-y3jqg8-agmfix', target_commit: '9a12d60bb2', scope: 'mission 追問／續作契約', status: 'pending', created_at: '2026-09-14T00:55:00.000Z' },
]

export async function fetchRebuildRequests(): Promise<RebuildRequest[] | null> {
  if (rawTransport.mock) return pendingRebuilds(MOCK_ROWS)
  try {
    const raw = await rawTransport.request('GET', '/supervisor/approvals')
    const rows = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>).approvals : null
    if (!Array.isArray(rows)) return null
    return pendingRebuilds(rows.map(toRow).filter((r): r is RebuildRequest => r !== null))
  } catch (e) {
    if (e instanceof ApiError) return null
    throw e
  }
}
