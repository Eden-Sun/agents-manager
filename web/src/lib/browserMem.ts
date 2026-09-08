import type { BrowserMem } from '../api/types'
import { humanBytes } from '../components/MemBadge'

/**
 * 瀏覽器分頁的警戒線（2026-09-08）：Chrome / ego 開到這個數量以上，RAM 通常就是被它們吃掉的，
 * 不是 agent。用 renderer process 數當分頁數，一個 renderer 粗估 100–300 MB。
 */
export const TABS_WARN = 30

/** `Chrome 12 分頁 2.1G · ego 6 分頁 700M`；沒有瀏覽器就空字串。 */
export function browsersLine(bs: BrowserMem[]): string {
  return bs.map((b) => `${b.name} ${b.tabs} 分頁 ${humanBytes(b.bytes)}`).join(' · ')
}

export function tabsTotal(bs: BrowserMem[]): number {
  return bs.reduce((n, b) => n + b.tabs, 0)
}
