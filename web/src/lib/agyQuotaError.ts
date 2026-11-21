import type { ToolStatus } from '../api/types'
import { t } from './i18n'

export function agyQuotaErrorText(err: NonNullable<ToolStatus['quota_error']>): string {
  switch (err.reason) {
    case 'timeout':
      return t('探測逾時（agy 一直沒有回應）')
    case 'unreadable':
      return t('agy /usage 的輸出讀不懂')
    case 'exit':
      return t('agy 執行失敗')
    case 'pane':
      return t('這台主機的 herdr pane 開不起來')
    case 'not_connected':
      return t('主機尚未連線')
    default:
      return err.message || t('原因不明')
  }
}
