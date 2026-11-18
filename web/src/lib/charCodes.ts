/** 控制字元的判斷用字元碼比對（不用字面的控制字元 regex，eslint no-control-regex 會擋）。 */

/** ESC（0x1b），用來組 ANSI 轉義序列的 RegExp：`new RegExp(ESC + '\\[…')`。 */
export const ESC = String.fromCharCode(0x1b)

/** 字串裡（UTF-16 碼元）有沒有任何一個碼元落在 [lo, hi]（含）之間。 */
export function hasCodeIn(s: string, lo: number, hi: number): boolean {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i)
    if (c >= lo && c <= hi) return true
  }
  return false
}
