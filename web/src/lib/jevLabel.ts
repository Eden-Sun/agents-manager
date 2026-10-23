/** 「Jev 建議」統一標示（issue #1912）用的字樣與機率換算；元件在 `components/JevLabel.tsx`。 */
export const JEV_FULL_NAME = 'Jev（TypeSafe 型別判斷）'
export const JEV_ADVISORY_NOTE = '只是建議，不會自動動作'

/** 0–1 的機率寫成「82%」；不是 0–1 的有限數字回 `null`。 */
export function jevProbText(prob: number | null | undefined): string | null {
  if (typeof prob !== 'number' || !Number.isFinite(prob) || prob < 0 || prob > 1) return null
  return `${Math.round(prob * 100)}%`
}
