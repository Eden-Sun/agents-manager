import type { ToolStatus } from '../api/types'

/**
 * agy 額度探測這一輪失敗的原因（daemon 的 `tools.agy.quota_error`）翻成人話。
 * 只在「已登入、但額度暫時拿不到」那一格用：登入與否看 `logged_in`，兩件事分開顯示，
 * 不再讓探測失敗看起來像「背景查詢中」或「未登入」。
 */
export function agyQuotaErrorText(err: NonNullable<ToolStatus['quota_error']>): string {
  switch (err.reason) {
    case 'timeout':
      return '探測逾時（agy 一直沒有回應）'
    case 'unreadable':
      return 'agy /usage 的輸出讀不懂'
    case 'exit':
      return 'agy 執行失敗'
    case 'pane':
      return '這台主機的 herdr pane 開不起來'
    case 'not_connected':
      return '主機尚未連線'
    default:
      return err.message || '原因不明'
  }
}
