import { AgmReviewBox } from './AgmReviewBox'
import { UpdateChangelog } from './UpdateChangelog'

/**
 * agy、grok 更新確認框上方的「改了什麼＋分析」，跟 claude／codex／herdr 同一個左右分欄（#1930）。
 * 兩顆 chip 與設定頁「更新檢查」的 agy 框共用，開著才掛（不開就不抓）。`refreshKey` 是「請 AGM 解析」派出去之後的重讀序號。
 */
export function UpstreamChangelogBox({ kind, host, from, to, refreshKey = 0 }: { kind: 'agy' | 'grok'; host: string; from: string | null; to: string; refreshKey?: number }) {
  return (
    <div className="update-split">
      <UpdateChangelog kind={kind} host={host} from={from} to={to} />
      <AgmReviewBox kind={kind} host={host} from={from} to={to} refreshKey={refreshKey} />
    </div>
  )
}
