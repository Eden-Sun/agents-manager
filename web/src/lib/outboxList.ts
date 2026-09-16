/** 檔案暫存區裡「bot 給你的檔案」那一段的純邏輯：怎麼講一個檔案、還剩多久、怎麼講「沒得列」。 */
import type { OutboxFile } from '../api'

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
 * 「這顆 bot 沒有可列的檔案」有好幾種原因，而且都**不是錯誤**：講清楚是哪一種，
 * 使用者才不會對著空白區域猜自己是不是按錯了。
 */
export function emptyReason(reason: string | null, botSelected: boolean): string {
  if (!botSelected) return '先選一顆 bot，這裡會列出它交給你的檔案。'
  if (reason === 'outbox_remote') return '這顆 bot 在遠端主機上，它的檔案不在這台機器，列不出來。'
  // daemon 902a85c：outbox 或 bot 那一層是符號連結、擁有者不是資料目錄的使用者——為了安全不列也不給下載，不能說成「還沒有檔案」。
  if (reason === 'outbox_untrusted') return '這顆 bot 的 outbox 不是一般資料夾（符號連結或擁有者不對），為了安全不列出、也不給下載。'
  return '還沒有檔案。bot 把要給你的檔案放進 $AM_OUTBOX 之後按重整就看得到；放進去 1 小時後會自動清掉。'
}

/** 新的排前面；同一秒的依名字排，避免每次重整順序在跳。 */
export function orderFiles(files: OutboxFile[]): OutboxFile[] {
  return [...files].sort((a, b) => b.modified - a.modified || a.name.localeCompare(b.name))
}
