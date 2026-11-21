import { t } from './i18n'

export function agoLabel(iso: string, now: number = Date.now()): string {
  const timestamp = Date.parse(iso)
  if (Number.isNaN(timestamp)) return ''
  const m = Math.max(0, Math.floor((now - timestamp) / 60_000))
  if (m < 1) return t('剛剛')
  if (m < 60) return t('{n} 分鐘前', { n: m })
  const h = Math.floor(m / 60)
  if (h < 48) return t('{n} 小時前', { n: h })
  return t('{n} 天前', { n: Math.floor(h / 24) })
}
