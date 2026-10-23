/**
 * 聊天室裡 daemon 留的 Jev 系統訊息（SPEC §4.3c 完成回報等，issue #1912）：文字以「🤖 Jev 建議（TypeSafe 型別判斷，機率 N%，只是建議，不會自動動作）：」
 * 開頭（🤖、括號、機率都可省，也接受「Jev 建議 82%：」），整段開頭被吃掉，剩下的是正文。認得就回機率與剩下的正文，聊天室用統一的 `JevLabel` 取代那段開頭；不是就 `null`，照原文印。
 */
export interface JevNotice {
  /** 0–1；開頭沒寫機率就是 `null`。 */
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
