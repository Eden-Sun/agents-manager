import { getLang } from '../lib/i18n'

export function L(zh: string, en: string): string {
  return getLang() === 'en' ? en : zh
}
