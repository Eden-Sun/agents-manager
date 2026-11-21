import type { BrowserMem } from '../api/types'
import { humanBytes } from '../components/memFormat'
import { t } from './i18n'

export const TABS_WARN = 30

export function browsersLine(bs: BrowserMem[]): string {
  return bs.map((b) => t('{name} {n} 分頁 {bytes}', { name: b.name, n: b.tabs, bytes: humanBytes(b.bytes) })).join(' · ')
}

export function tabsTotal(bs: BrowserMem[]): number {
  return bs.reduce((n, b) => n + b.tabs, 0)
}
