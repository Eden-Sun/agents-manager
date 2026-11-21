export interface JevNotice {
  prob: number | null
  rest: string
}

const LEAD = /^\s*(?:🤖\s*)?Jev 建議\s*(?:[（(]([^）)]*)[）)])?\s*(?:(\d{1,3}(?:\.\d+)?)\s*%)?\s*[：:，,]?\s*/

export function parseJevNotice(content: string): JevNotice | null {
  const m = content.match(LEAD)
  if (!m) return null
  const inParen = m[1]?.match(/機率\s*(\d{1,3}(?:\.\d+)?)\s*%/)?.[1]
  const raw = inParen ?? m[2]
  const pct = raw === undefined ? null : Number(raw)
  return { prob: pct !== null && pct >= 0 && pct <= 100 ? pct / 100 : null, rest: content.slice(m[0].length) }
}
