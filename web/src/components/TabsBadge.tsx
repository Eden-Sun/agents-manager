import { LOCAL_HOST } from '../api/types'
import { humanBytes } from './MemBadge'
import { browsersLine, TABS_WARN, tabsTotal } from '../lib/browserMem'
import { useStore } from '../store/store'

/**
 * 本機瀏覽器（Chrome / ego）的分頁總數與它們吃的 RAM，直接列在 RAM 那格底下：
 * 「分頁 16 / 2.1G」。之前只在超線時才在 RAM 格冒一個小字，使用者要看得點開明細——
 * 這個數字跟 pane 數一樣是「現在開著多少東西」，就該一直在。沒有瀏覽器就整格不出現。
 */
export function TabsBadge({ host = LOCAL_HOST }: { host?: string }) {
  const browsers = useStore((s) => s.mem?.hosts.find((h) => h.host === host)?.browsers ?? null)
  if (!browsers || browsers.length === 0) return null
  const tabs = tabsTotal(browsers)
  const bytes = browsers.reduce((n, b) => n + b.bytes, 0)
  const hot = tabs >= TABS_WARN
  const tip = [
    browsersLine(browsers),
    hot ? `超過 ${TABS_WARN} 個分頁，RAM 多半是被瀏覽器吃掉的，關一些。` : '',
    '（瀏覽器不算在上面的 RAM 裡；一個 renderer 當一個分頁。）',
  ].filter(Boolean)
  return (
    <span className={`tabs-badge${hot ? ' hot' : ''}`} title={tip.join('\n')}>
      <span className="tabs-k">分頁</span>
      <span className="tabs-v">
        {tabs}
        <span className="tabs-sep">/</span>
        {humanBytes(bytes)}
      </span>
    </span>
  )
}
