import { t } from './i18n'

export function newDirNameProblem(raw: string): string | null {
  const name = raw.trim()
  if (!name) return t('請輸入資料夾名稱')
  if (name === '.' || name === '..') return t('名稱不能是 . 或 ..')
  if (name.includes('/')) return t('名稱不能含「/」（一次只建一層；要進到別層請先在上面的清單走進去）')
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u2064\u2066-\u2069\ufeff]/.test(name)) return t('名稱不能含控制或看不見的字元')
  if (new TextEncoder().encode(name).length > 255) return t('名稱太長（最多 255 位元組）')
  return null
}
