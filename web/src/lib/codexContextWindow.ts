/**
 * gpt-6.1-sol 的 context 分母固定 500k（使用者 2026-10-14）。codex CLI 自己算的 `Context N% used` 與 rollout 的
 * `model_context_window` 都用 475k，不能拿來當百分比。
 *
 * 規則：
 * - 只套在 **codex** bot，而且 bot 的**有效**模型等於 `gpt-6.1-sol`（不分大小寫）；`gpt-6-sol`、`gpt-5.6-sol`、
 *   Luna、Astra、claude、grok 一律照既有規則。
 * - 分子只用**實際** token（rollout 最近一筆的 `input_tokens`＝`prompt_cache.context_used_tokens`，沒有時用 statusLine 的
 *   `context_used_tokens`）。沒有實際 token 就不算百分比，**不**拿舊的百分比乘 500k（那個百分比是 CLI 用 475k 算的）。
 * - token 必須是有限、非負的數；0 合法（顯示 0%）。
 *
 * 純函式，不引入其他模組（`composerCost` 要用它，反過來不行）；格式化由呼叫端做。
 */

export const GPT_61_SOL_CONTEXT_WINDOW = 500_000

export function isGpt61Sol(model: string | null | undefined): boolean {
  return (model ?? '').trim().toLowerCase() === 'gpt-6.1-sol'
}

/** bot 的有效模型：正在跑的 `runtime_model` 有值（去空白後非空）就用它，否則用設定值 `model`。 */
export function effectiveModelOf(runtimeModel: string | null | undefined, botModel: string | null | undefined): string | null {
  const runtime = runtimeModel?.trim()
  if (runtime) return runtime
  const configured = botModel?.trim()
  return configured || null
}

/** 這個 bot 的 context 要不要改用 500k 分母：codex 且有效模型是 gpt-6.1-sol。 */
export function solWindowApplies(kind: string | null | undefined, model: string | null | undefined): boolean {
  return kind === 'codex' && isGpt61Sol(model)
}

/** 適用的 bot 且有實際 token（有限、≥0）：`pct` 是 `tokens ÷ 500000`（未四捨五入）；其他情況回 null。 */
export function sol500kContext(
  kind: string | null | undefined,
  model: string | null | undefined,
  usedTokens: number | null | undefined,
): { pct: number; tokens: number } | null {
  if (!solWindowApplies(kind, model)) return null
  if (usedTokens == null || !Number.isFinite(usedTokens) || usedTokens < 0) return null
  return { pct: (usedTokens / GPT_61_SOL_CONTEXT_WINDOW) * 100, tokens: usedTokens }
}
