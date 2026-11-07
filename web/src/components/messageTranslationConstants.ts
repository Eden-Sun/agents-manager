/** 自動翻譯只管對話最後這幾則（加上新到的）；更早的不回填，免得一開對話就把整段歷史丟去翻（使用者 2026-11-06）。 */
export const AUTO_TRANSLATE_RECENT = 10

/** 美元：不到一分錢多給位數，免得全寫成 $0.00。 */
export function usd(n: number): string {
  return n >= 0.01 ? `$${n.toFixed(3)}` : `$${n.toFixed(5)}`
}
