/**
 * 代發／轉述訊息（AGM 交辦、bot 之間互轉）預設收合，只露一行摘要。
 *
 * 2026-09-14 使用者：「這種 agm 交辦的訊息都不用全顯示，收合就好」。這類訊息動輒十幾行
 * （核准 id、流程、驗證清單），是寫給 bot 看的，攤開會把使用者自己的對話擠出畫面。
 * 摘要取第一個非空行——交辦的第一行本來就是標題（「AGM 交辦：部署 origin/main …」）。
 */

/** 摘要最多幾個字（以 code point 計，中英混排才不會切在半個字上）。 */
export const RELAY_PREVIEW_MAX = 90

export interface RelayPreview {
  /** 收合時顯示的那一行。 */
  text: string
  /** 全文比摘要長，才值得收合；短訊息原樣顯示、不給「展開」鍵。 */
  truncated: boolean
  /** 全文字數（code point），放在「展開」鍵上。 */
  length: number
}

export function relayPreview(content: string): RelayPreview {
  const full = content.trim()
  const length = [...full].length
  const first = full.split('\n').find((l) => l.trim() !== '')?.trim() ?? ''
  const chars = [...first]
  const clipped = chars.length > RELAY_PREVIEW_MAX
  const text = clipped ? `${chars.slice(0, RELAY_PREVIEW_MAX).join('')}…` : first
  return { text, truncated: clipped || first !== full, length }
}
