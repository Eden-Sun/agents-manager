/**
 * claude／codex「上游有新版、磁碟上還沒有」的通知（issue #707，daemon `upstream_update.rs`）。
 * 跟 header 的「重啟套用」是兩件事：這裡是還沒下載／還沒安裝，重啟換不到任何東西，所以只跳一則通知、不進批次重啟。
 *
 * 每個新版本每個瀏覽器只跳一次：看過的版本記在 localStorage。daemon 那邊也只推一次，但推的當下沒開網頁就會錯過，
 * 所以開機時再讀一次 `GET /api/upstream-updates` 補上。抓不到上游（`notify: "error"`）照樣講，只在 daemon 推的那一次。
 */
import { writeShared } from './mobilePreview'

const KEY = 'am.upstreamUpdate.seen'

export interface UpstreamItem {
  kind: string
  latest: string | null
  hasUpdate: boolean
  notify: 'update' | 'error' | null
  text: string | null
}

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null
}

export function parseUpstreamItem(raw: unknown): UpstreamItem | null {
  if (!isRec(raw) || typeof raw.kind !== 'string') return null
  const notify = raw.notify === 'update' || raw.notify === 'error' ? raw.notify : null
  return {
    kind: raw.kind,
    latest: typeof raw.latest_version === 'string' ? raw.latest_version : null,
    hasUpdate: raw.has_update === true,
    notify,
    text: typeof raw.text === 'string' && raw.text ? raw.text : null,
  }
}

/** 要跳哪一則；`seen` 是這個瀏覽器看過的版本（kind → 版本）。 */
export function upstreamNotice(item: UpstreamItem, seen: Record<string, string>): { kind: 'info' | 'error'; text: string } | null {
  if (!item.text) return null
  if (item.notify === 'error') return { kind: 'error', text: item.text }
  if (item.hasUpdate && item.latest && seen[item.kind] !== item.latest) return { kind: 'info', text: item.text }
  return null
}

function readSeen(): Record<string, string> {
  try {
    const v: unknown = JSON.parse(localStorage.getItem(KEY) ?? '{}')
    return isRec(v) ? (Object.fromEntries(Object.entries(v).filter(([, x]) => typeof x === 'string')) as Record<string, string>) : {}
  } catch {
    return {}
  }
}

function markSeen(kind: string, version: string) {
  writeShared(() => {
    try {
      localStorage.setItem(KEY, JSON.stringify({ ...readSeen(), [kind]: version }))
    } catch {
      /* 存不進去就是下次再跳一次 */
    }
  })
}

type Notify = (kind: 'info' | 'error', text: string, action?: { label: string; run: () => void }) => void

/** `upstream_update` 幀或開機讀到的一筆。 */
export function applyUpstreamItem(raw: unknown, notify: Notify) {
  const item = parseUpstreamItem(raw)
  if (!item) return
  const n = upstreamNotice(item, readSeen())
  if (!n) return
  if (n.kind === 'info' && item.latest) markSeen(item.kind, item.latest)
  // 帶一顆「知道了」讓它停久一點（純資訊通知 4 秒就消失，這則是要人去處理的）。
  notify(n.kind, n.text, { label: '知道了', run: () => {} })
}

export async function loadUpstreamUpdates(fetchItems: () => Promise<unknown>, notify: Notify) {
  try {
    const r = await fetchItems()
    const items = isRec(r) && Array.isArray(r.items) ? r.items : []
    // 開機只補「有新版」：抓不到上游的那一則 daemon 已經在變化的當下推過，每次重整都跳會變成噪音。
    for (const raw of items) if (isRec(raw)) applyUpstreamItem({ ...raw, notify: null }, notify)
  } catch {
    /* 舊 daemon 沒有這支、或暫時連不上：下一幀 `upstream_update` 會帶到 */
  }
}
