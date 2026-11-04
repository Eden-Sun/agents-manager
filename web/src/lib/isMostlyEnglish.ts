/**
 * 訊息「主要是英文」嗎：去掉程式碼區塊與行內 code、網址之後，拉丁字母要佔大多數、CJK 很少。
 * 給對話裡的「翻譯」按鈕判斷要不要出現；純函式。
 */
export function isMostlyEnglish(text: string): boolean {
  const prose = text
    .replace(/```[\s\S]*?(```|$)/g, ' ')
    .replace(/`[^`\n]*`/g, ' ')
    .replace(/https?:\/\/\S+/g, ' ')
  const latin = (prose.match(/[A-Za-z]/g) ?? []).length
  const cjk = (prose.match(/[぀-ヿ㐀-鿿가-힯]/g) ?? []).length
  if (latin < 20) return false
  return cjk * 10 <= latin
}
