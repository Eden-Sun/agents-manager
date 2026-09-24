/**
 * 左上角「重建 N/門檻」那顆 chip 的資料（使用者 2026-09-14）。
 *
 * 唯讀，走既有的 `GET /api/supervisor/approvals` 與 `GET /api/supervisor`（只取 `last_deploy.at`，
 * 用來排除上次上線以前的舊申請）；計數規則在 `lib/rebuildCount.ts`，與
 * `scripts/ops/daemon-update-kick.sh` 同一套。端點不存在（舊 daemon）或 daemon 回錯時回 `rows: null`，
 * chip 整顆不出現——它不是錯誤面板。
 *
 * **連不上 daemon 是第三種情況**（issue #531）：以前只吞 `ApiError`，`fetch` 丟的 `TypeError` 會往外拋，
 * 呼叫端的 `void refresh()` 沒接，daemon 每重啟一次就每 30 秒一則 unhandled rejection，而 chip 停在
 * 斷線前的數字、看起來像現況。現在一律不拋，並用 `offline` 把「問不到」跟「daemon 說沒有」分開：
 * 前者留著上一次的數字但標成過期，後者才真的收掉。
 */
import { rawTransport } from './index'
import { ApiError } from './types'
import { pendingRebuilds, type RebuildRequest } from '../lib/rebuildCount'

/** daemon 還沒有「門檻是多少」的欄位，所以前端也寫死同一個值（見 docs/UI-DECISIONS.md）。 */
export const REBUILD_THRESHOLD = 3

/** 最早一筆申請等超過這麼多分鐘也不等整點（`AGM_REBUILD_MAX_WAIT_MIN`，使用者 2026-09-15）。 */
export const REBUILD_MAX_WAIT_MIN = 30

function toRow(v: unknown): RebuildRequest | null {
  if (typeof v !== 'object' || v === null) return null
  const o = v as Record<string, unknown>
  if (o.purpose !== 'rebuild') return null
  const s = (k: string): string => (typeof o[k] === 'string' ? (o[k] as string) : '')
  const id = s('id')
  if (!id) return null
  return {
    id,
    requester: s('requester'),
    target_commit: s('target_commit'),
    scope: s('scope'),
    status: s('status'),
    created_at: s('created_at'),
    // daemon 不會把過期的核准改狀態，所以要自己看這一欄（跟 kick 腳本同一條規則）。
    expires_at: s('expires_at') || null,
  }
}

/** mock 的假資料：`VITE_MOCK=1` 下要看得到 chip 與清單長什麼樣，不然只能對真 daemon 看。 */
const MOCK_ROWS: RebuildRequest[] = [
  { id: 'm1', requester: 'agents-manager-k8bw2f', target_commit: '4d5fe88aa1', scope: 'codex 改 effort 不該重啟', status: 'pending', created_at: '2026-09-14T02:10:00.000Z' },
  { id: 'm2', requester: 'agents-manager-mkng2n', target_commit: '4d5fe88aa1', scope: '瘦身 C：DB migrate 只支援現行 schema', status: 'approved', created_at: '2026-09-14T01:40:00.000Z' },
  { id: 'm3', requester: 'ag-man-y3jqg8-agmfix', target_commit: '9a12d60bb2', scope: 'mission 追問／續作契約', status: 'pending', created_at: '2026-09-14T00:55:00.000Z' },
]

/** 上次成功上線的時間（daemon 960ba06 起的 `last_deploy.at`）。問不到就 `null` = 不濾時間。 */
async function lastDeployAt(): Promise<string | null> {
  try {
    const raw = await rawTransport.request('GET', '/supervisor')
    if (typeof raw !== 'object' || raw === null) return null
    const d = (raw as Record<string, unknown>).last_deploy
    if (typeof d !== 'object' || d === null) return null
    const at = (d as Record<string, unknown>).at
    return typeof at === 'string' && at ? at : null
  } catch {
    return null
  }
}

/** 總管自己那顆 bot 的 id（`GET /api/supervisor`）；問不到就 `null`。 */
export async function supervisorBotId(): Promise<string | null> {
  if (rawTransport.mock) return 'mock-agm'
  try {
    const raw = await rawTransport.request('GET', '/supervisor')
    if (typeof raw !== 'object' || raw === null) return null
    const id = (raw as Record<string, unknown>).bot_id
    return typeof id === 'string' && id ? id : null
  } catch {
    return null
  }
}

/** 一次查詢的結果。`rows: null` ＝這一次沒有名單可用；`offline` ＝原因是連不上，不是 daemon 說沒有。 */
export interface RebuildSnapshot {
  rows: RebuildRequest[] | null
  offline: boolean
}

export async function fetchRebuildRequests(): Promise<RebuildSnapshot> {
  if (rawTransport.mock) return { rows: pendingRebuilds(MOCK_ROWS), offline: false }
  try {
    const [raw, since] = await Promise.all([rawTransport.request('GET', '/supervisor/approvals'), lastDeployAt()])
    const rows = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>).approvals : null
    if (!Array.isArray(rows)) return { rows: null, offline: false }
    return { rows: pendingRebuilds(rows.map(toRow).filter((r): r is RebuildRequest => r !== null), since), offline: false }
  } catch (e) {
    // daemon 有回（404 舊 daemon、500…）＝它說了算，chip 收掉；連不上（`TypeError: Failed to fetch`、
    // abort）只代表這一次問不到，不改數字，改標過期。兩種都不往外拋。
    return { rows: null, offline: !(e instanceof ApiError) }
  }
}
