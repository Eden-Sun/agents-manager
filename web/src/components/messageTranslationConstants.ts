/** 自動翻譯只管對話最後這幾則（加上新到的）；更早的不回填，免得一開對話就把整段歷史丟去翻（使用者 2026-11-06）。 */
export const AUTO_TRANSLATE_RECENT = 10

/** 美元：不到一分錢多給位數，免得全寫成 $0.00。 */
export function usd(n: number): string {
  return n >= 0.01 ? `$${n.toFixed(3)}` : `$${n.toFixed(5)}`
}

/**
 * 譯文下方標示來源的短名：去掉 `claude-` 前綴後取第一段、首字大寫（`claude-sonnet-5-5`→`Sonnet`、
 * `claude-haiku-5-5`→`Haiku`、`fable-*`→`Fable`）。OpenAI 是 `gpt-<版本>-<名稱>` 取名稱首字大寫
 * （`gpt-6-luna`→`Luna`、`gpt-6.2-sol`→`Sol`；使用者 2026-11-18：「UI 不是 by gpt 而是 by luna」）。
 * `opencc-s2twp`（本地簡轉繁）回 `OpenCC`。
 */
export function translationModelName(model: string): string {
  if (isLocalTranslation(model)) return 'OpenCC'
  const openai = /^gpt-\d+(?:\.\d+)*-([^-]+)/.exec(model)
  const first = openai ? openai[1] : (model.replace(/^claude-/, '').split('-')[0] ?? '')
  return first.charAt(0).toUpperCase() + first.slice(1)
}

/** 本地 OpenCC 轉換不計費，標示時不帶金額。 */
export function isLocalTranslation(model: string): boolean {
  return model.startsWith('opencc')
}
