/**
 * 訊息「主要是英文」嗎：去掉程式碼區塊與行內 code、網址之後，以行/段為單位判斷。
 * 英文主導的行（拉丁字母多、CJK 少）佔比要高，且拉丁字母總數要足夠。
 * 給對話裡的「翻譯」按鈕判斷要不要出現；純函式。
 */
export function isMostlyEnglish(text: string): boolean {
  const prose = text
    .replace(/```[\s\S]*?(```|$)/g, ' ')
    .replace(/`[^`\n]*`/g, ' ')
    .replace(/https?:\/\/\S+/g, ' ')

  const lines = prose.split('\n');
  let englishDominantLineCount = 0;
  let nonEmptyLineCount = 0;
  let totalLatinCount = 0;

  for (const line of lines) {
    const trimmedLine = line.trim();
    if (trimmedLine === '') continue;
    nonEmptyLineCount++;

    const latin = (trimmedLine.match(/[A-Za-z]/g) ?? []).length;
    const cjk = (trimmedLine.match(/[぀-ヿ㐀-鿿가-힯]/g) ?? []).length;

    // 判斷該行是否為「英文主導」
    if (latin > 10 * cjk) {
      englishDominantLineCount++;
      totalLatinCount += latin;
    }
  }

  if (nonEmptyLineCount === 0) return false;
  // 維持原本至少 20 個拉丁字母的門檻，並要求英文主導行佔比 > 50%
  return totalLatinCount > 20 && (englishDominantLineCount / nonEmptyLineCount) > 0.5;
}
