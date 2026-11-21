import { ApiError } from '../api/types'
import { msg, t, tr } from './i18n'

export type AttachmentFailure = 'gone' | 'error'

export function loadFailure(e: unknown): AttachmentFailure {
  return e instanceof ApiError && (e.status === 404 || e.status === 410) ? 'gone' : 'error'
}

export const GONE_LABEL = msg('已清除')

export function failureLabel(f: AttachmentFailure): string {
  return tr(f === 'gone' ? GONE_LABEL : msg('無法載入'))
}

export function failureHint(f: AttachmentFailure): string {
  return f === 'gone'
    ? t('附件檔已清除（bot 刪除後只保留 7 天）；這裡只留檔名與當時的路徑。')
    : t('附件暫時載入失敗，稍後再開一次。')
}
