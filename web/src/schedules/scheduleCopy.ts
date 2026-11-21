import { t } from '../lib/i18n'

export function copyScheduleName(name: string): string {
  const suffix = t('（副本）')
  const chars = Array.from(name.trim())
  while (chars.join('').length > 80 - suffix.length) chars.pop()
  return `${chars.join('')}${suffix}`
}
