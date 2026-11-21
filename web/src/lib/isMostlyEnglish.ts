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

    if (latin > 10 * cjk) {
      englishDominantLineCount++;
      totalLatinCount += latin;
    }
  }

  if (nonEmptyLineCount === 0) return false;
  return totalLatinCount > 20 && (englishDominantLineCount / nonEmptyLineCount) > 0.5;
}
