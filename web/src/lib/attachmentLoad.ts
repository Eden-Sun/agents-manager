import { ApiError } from '../api/types'

/** 附件位元組抓不到的兩種：檔案已被清掉（404／410），或暫時的（斷線、5xx、token）。 */
export type AttachmentFailure = 'gone' | 'error'

/**
 * daemon 對讀不到的附件一律回 404（`get_attachment`）：bot 刪掉之後附件搬進 `bots-trash`，放超過 7 天
 * （`bot_trash::KEEP_DAYS`）或回收區超過總量上限就被清掉，之後從「最近刪除」復原的對話裡，縮圖的檔案已經不在了。
 */
export function loadFailure(e: unknown): AttachmentFailure {
  return e instanceof ApiError && (e.status === 404 || e.status === 410) ? 'gone' : 'error'
}

export const GONE_LABEL = '已清除'

export function failureLabel(f: AttachmentFailure): string {
  return f === 'gone' ? GONE_LABEL : '無法載入'
}

export function failureHint(f: AttachmentFailure): string {
  return f === 'gone'
    ? '附件檔已清除（bot 刪除後只保留 7 天）；這裡只留檔名與當時的路徑。'
    : '附件暫時載入失敗，稍後再開一次。'
}
