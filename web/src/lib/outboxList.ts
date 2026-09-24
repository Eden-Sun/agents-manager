/** 檔案暫存區裡「bot 給你的檔案」那一段的純邏輯：怎麼講一個檔案、還剩多久、怎麼講「沒得列」。 */
import type { OutboxFile } from '../api'
import { ApiError } from '../api/types'
import type { Turn } from '../api/types'

/** `18 KB` / `1.1 MB`：一行要看得懂，個位數才給小數。跟 MemBadge 的 `humanBytes` 不同單位詞：
 *  那邊是一格寬度的 RAM（`1.4G`），這邊是檔案大小，使用者看的是「下載會多大」。 */
export function fileSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const mb = bytes / 1024 ** 2
  if (mb >= 1) return `${mb < 10 ? mb.toFixed(1) : Math.round(mb)} MB`
  const kb = bytes / 1024
  if (kb >= 1) return `${kb < 10 ? kb.toFixed(1) : Math.round(kb)} KB`
  return `${Math.round(bytes)} B`
}

/**
 * 這個檔案現在還剩幾秒。daemon 給的是**讀清單那一刻**的剩餘秒數，這裡扣掉之後經過的時間——
 * 不拿 `expires_at` 跟瀏覽器的時鐘比，手機時鐘偏了幾分鐘也不會算錯。
 */
export function remainingNow(file: OutboxFile, fetchedAt: number, now = Date.now()): number {
  const elapsed = Math.max(0, Math.floor((now - fetchedAt) / 1000))
  return Math.max(0, file.remainingSecs - elapsed)
}

/** `剩 42 分鐘`：無條件進位，剩 30 秒也講「剩 1 分鐘」；到期了講「即將清除」（清理每 10 分鐘才跑一次）。 */
export function remainingLabel(secs: number): string {
  if (!Number.isFinite(secs) || secs <= 0) return '即將清除'
  return `剩 ${Math.ceil(secs / 60)} 分鐘`
}

/**
 * 下載失敗要講哪一句，以及這一列是不是已經沒了（issue #547）。
 *
 * 404 ＝ 檔案被清掉了：outbox 的檔案放進去一小時後由 AGM 的 gc 掃掉（每 10 分鐘一輪），而清單只在換 bot、
 * 按 ↻ 或回合結束時重讀，所以過期的那幾列會一直留在畫面上。`gone` 讓呼叫端當場把它拿掉並重讀一次。
 * 其餘的把 daemon 的原因講成人話——以前這裡只拿得到 `res.statusText`（「Not Found」「Conflict」）。
 */
export function downloadFailure(name: string, e: unknown): { text: string; gone: boolean } {
  const plain = (text: string) => ({ text, gone: false })
  if (!(e instanceof ApiError)) return plain(`下載「${name}」失敗：${e instanceof Error ? e.message : String(e)}`)
  if (e.status === 404) {
    return { text: `「${name}」已經不在了：outbox 的檔案放進去一小時後會自動清掉。`, gone: true }
  }
  const reason = String(e.body.reason ?? e.body.error ?? '')
  if (reason === 'file_too_large') {
    const big = typeof e.body.size === 'number' ? `（${fileSize(e.body.size)}）` : ''
    const max = typeof e.body.max === 'number' ? fileSize(e.body.max) : '下載上限'
    return plain(`「${name}」太大${big}，超過 ${max}，這裡下載不了——請 bot 換個小一點的，或到那台機器上拿。`)
  }
  if (reason === 'outbox_remote') return plain(`「${name}」在遠端主機上，不在這台機器，下載不了。`)
  return plain(`下載「${name}」失敗：${e.message}（HTTP ${e.status}）`)
}

/** 讀不到清單（網路、500、404）：畫面上的 `reason`，不是 daemon 回的（#234）。 */
export const LOAD_FAILED = 'load_failed'

export type OutboxList = { dir: string; reason: string | null; ttlSecs: number; files: OutboxFile[] }

/**
 * 讀一次清單。讀不到回 `reason: 'load_failed'` 的空清單，**不是** `reason: null`：以前 catch 把任何錯誤都當成「還沒有檔案」，
 * bot 明明放了檔案，畫面卻叫使用者「把檔案放進 $AM_OUTBOX」，讀不到與真的沒檔分不開（#234）。
 * 這是附加資訊，不跳紅字（每個回合結束都會重讀，toast 會變成連珠炮）；畫面在空白處講清楚並指路去按 ↻。
 */
export async function readOutbox(fetchList: () => Promise<OutboxList>): Promise<OutboxList> {
  try {
    const out = await fetchList()
    return { ...out, files: orderFiles(out.files) }
  } catch {
    return { dir: '', reason: LOAD_FAILED, ttlSecs: 0, files: [] }
  }
}

/**
 * 「這顆 bot 沒有可列的檔案」有好幾種原因，多半**不是錯誤**：講清楚是哪一種，
 * 使用者才不會對著空白區域猜自己是不是按錯了。讀不到清單（[`LOAD_FAILED`]）是唯一的例外，要講得跟「沒有檔案」不一樣。
 */
export function emptyReason(reason: string | null, botSelected: boolean): string {
  if (!botSelected) return '先選一顆 bot，這裡會列出它交給你的檔案。'
  if (reason === LOAD_FAILED) return '讀不到這顆 bot 的檔案清單（daemon 沒有回應或出錯），不代表沒有檔案。按上面的 ↻ 重新讀取。'
  if (reason === 'outbox_remote') return '這顆 bot 在遠端主機上，它的檔案不在這台機器，列不出來。'
  // daemon 902a85c：outbox 或 bot 那一層是符號連結、擁有者不是資料目錄的使用者——為了安全不列也不給下載，不能說成「還沒有檔案」。
  if (reason === 'outbox_untrusted') return '這顆 bot 的 outbox 不是一般資料夾（符號連結或擁有者不對），為了安全不列出、也不給下載。'
  return '還沒有檔案。bot 把要給你的檔案放進 $AM_OUTBOX 之後按重整就看得到；放進去 1 小時後會自動清掉。'
}

/** 新的排前面；同一秒的依名字排，避免每次重整順序在跳。 */
export function orderFiles(files: OutboxFile[]): OutboxFile[] {
  return [...files].sort((a, b) => b.modified - a.modified || a.name.localeCompare(b.name))
}

/**
 * 這顆 bot 最近一個**已經結束**的回合（`id:completed_at`），沒有就是空字串。
 *
 * 清單原本只在換 bot 或按 ↻ 時重讀：bot 回「放好了」的那一刻清單不會動，使用者要切個頁面才看得到
 * （2026-09-16 使用者：「為何沒有馬上出現／要切換頁面後才看見」）。bot 放檔一定發生在回合裡，
 * 所以回合一結束就重讀一次——這是事件，不是輪詢，「不每秒掃目錄」那條取捨不變。
 */
export function lastSettledTurnKey(turns: Record<string, Pick<Turn, 'id' | 'status' | 'completed_at'>> | undefined): string {
  let best: { id: string; at: string } | null = null
  for (const t of Object.values(turns ?? {})) {
    if (!t.completed_at || t.status === 'in_flight' || t.status === 'queued') continue
    if (!best || t.completed_at > best.at) best = { id: t.id, at: t.completed_at }
  }
  return best ? `${best.id}:${best.at}` : ''
}

/**
 * 滑過去就預覽的圖檔（使用者 2026-09-18：「圖檔我要可以 hover preview」）。
 * 只認 daemon 下載白名單裡的點陣圖（PNG/JPEG/GIF/WebP，API.md）；SVG 會被當 octet-stream，`<img>` 畫不出來，不列。
 */
export function isPreviewableImage(name: string): boolean {
  return /\.(png|jpe?g|gif|webp)$/i.test(name)
}

/**
 * 預覽框放在滑過的那一列**左邊**（清單貼著右側欄，右邊沒地方），上下夾在視窗內。
 * 左邊放不下（窄視窗）就改放在那一列下方。回傳 `left`/`top`，單位 px，給 `position: fixed` 用。
 */
export function previewPlacement(
  anchor: { left: number; top: number; bottom: number },
  box: { width: number; height: number },
  viewport: { width: number; height: number },
  gap = 8,
): { left: number; top: number } {
  const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(v, Math.max(lo, hi)))
  if (anchor.left - gap - box.width >= gap) {
    return { left: anchor.left - gap - box.width, top: clamp(anchor.top, gap, viewport.height - gap - box.height) }
  }
  return {
    left: clamp(anchor.left, gap, viewport.width - gap - box.width),
    top: clamp(anchor.bottom + gap, gap, viewport.height - gap - box.height),
  }
}
