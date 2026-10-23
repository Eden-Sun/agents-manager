import { getLang } from '../lib/i18n'

/** mock 示範資料的雙語文字：英文介面（`?lang=en` 或瀏覽器語言）出英文，其餘維持繁中。mock 檔不走 `t()`，這裡只挑原文。 */
export function L(zh: string, en: string): string {
  return getLang() === 'en' ? en : zh
}
