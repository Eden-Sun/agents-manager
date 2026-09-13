import { LOCAL_HOST } from '../api/types'
import { humanBytes } from './MemBadge'
import { browsersLine, TABS_WARN, tabsTotal } from '../lib/browserMem'
import { useStore } from '../store/store'
import { BrowserIcon } from './BrowserIcons'

/** 本機瀏覽器（Chrome / ego）各自的分頁數與 RAM，常駐在 RAM 格底下（跟 pane 數一樣是「現在開著多少」）；沒有瀏覽器就不出現。 */
export function TabsBadge({ host = LOCAL_HOST }: { host?: string }) {
  const browsers = useStore((s) => s.mem?.hosts.find((h) => h.host === host)?.browsers ?? null)
  if (!browsers || browsers.length === 0) return null
  const tabs = tabsTotal(browsers)
  const hot = tabs >= TABS_WARN
  const tip = [
    browsersLine(browsers),
    hot ? `超過 ${TABS_WARN} 個分頁，RAM 多半是被瀏覽器吃掉的，關一些。` : '',
    '（瀏覽器不算在上面的 RAM 裡；一個 renderer 當一個分頁。）',
  ].filter(Boolean)
  const ranked = [...browsers].sort((a, b) => {
    const rank = (n: string) => (n === 'Chrome' ? 0 : n === 'ego' ? 1 : 2)
    return rank(a.name) - rank(b.name) || a.name.localeCompare(b.name)
  })
  return (
    <span className={`tabs-badge${hot ? ' hot' : ''}`} title={tip.join('\n')}>
      {ranked.map((b) => (
        <span key={b.name} className="tabs-one" aria-label={`${b.name} ${b.tabs} 個分頁 ${humanBytes(b.bytes)}`}>
          <BrowserIcon name={b.name} />
          <span className="tabs-v">
            {b.tabs}
            {/* 自己的 span：手機抽屜標題列只放得下分頁數，CSS 要能收掉（mobile-rwd-round2-2026-09-08 問題 4）。 */}
            <span className="tabs-bytes">
              <span className="tabs-sep">/</span>
              {humanBytes(b.bytes)}
            </span>
          </span>
        </span>
      ))}
    </span>
  )
}
