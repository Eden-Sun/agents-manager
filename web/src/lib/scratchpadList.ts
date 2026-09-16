/** 檔案暫存區裡「這顆 bot 的 scratchpad」那一段的純邏輯：怎麼講一個檔案、怎麼講「沒得列」。 */
import type { ScratchpadFile } from '../api'

/** `18.0 KB` / `1.1 MB`：一行要看得懂，個位數才給小數。跟 MemBadge 的 `humanBytes` 不同單位詞：
 *  那邊是一格寬度的 RAM（`1.4G`），這邊是檔案大小，使用者看的是「下載會多大」。 */
export function fileSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const mb = bytes / 1024 ** 2
  if (mb >= 1) return `${mb < 10 ? mb.toFixed(1) : Math.round(mb)} MB`
  const kb = bytes / 1024
  if (kb >= 1) return `${kb < 10 ? kb.toFixed(1) : Math.round(kb)} KB`
  return `${Math.round(bytes)} B`
}

/** epoch 秒 → `剛剛` / `12 分鐘前` / `3 小時前` / `2 天前`。未來的時間（時鐘偏移）當成剛剛。 */
export function modifiedAgo(epochSeconds: number, now = Date.now()): string {
  if (!Number.isFinite(epochSeconds) || epochSeconds <= 0) return ''
  const m = Math.round((now - epochSeconds * 1000) / 60000)
  if (m < 1) return '剛剛'
  if (m < 60) return `${m} 分鐘前`
  const h = Math.round(m / 60)
  if (h < 48) return `${h} 小時前`
  return `${Math.round(h / 24)} 天前`
}

/**
 * 「這顆 bot 沒有可列的檔案」有好幾種原因，而且都**不是錯誤**：講清楚是哪一種，
 * 使用者才不會對著空白區域猜自己是不是按錯了。
 */
export function emptyReason(reason: string | null, botSelected: boolean): string {
  if (!botSelected) return '先選一顆 bot，這裡會列出它寫在 scratchpad 裡的檔案。'
  switch (reason) {
    case 'scratchpad_remote':
      return '這顆 bot 在遠端主機上，它的檔案不在這台機器，列不出來。'
    case 'scratchpad_no_session':
      return '這顆 bot 還沒跑過（或不是 claude），沒有 scratchpad。'
    case 'scratchpad_missing':
      return 'scratchpad 目錄還沒建立——bot 寫第一個檔案之後就會出現。'
    default:
      return 'scratchpad 裡還沒有檔案。bot 寫進去之後按重整就看得到。'
  }
}

/** 新的排前面；同一秒的依名字排，避免每次重整順序在跳。 */
export function orderFiles(files: ScratchpadFile[]): ScratchpadFile[] {
  return [...files].sort((a, b) => b.modified - a.modified || a.name.localeCompare(b.name))
}
