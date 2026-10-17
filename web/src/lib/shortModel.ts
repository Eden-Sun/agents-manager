/**
 * `claude-fable-5-1` → `fable`、`claude-opus-5` → `opus`：子 agent 從 argv / statusLine 讀回來的是
 * 完整 model id，跟旁邊使用者自己選的 `opus` 擺在一起就像另一個模型；側欄那格只放別名。
 * 認不得的原樣顯示，完整 id 留在 tooltip。
 */
export function shortModel(kind: string, model: string | null): string | null {
  if (!model) return null
  // codex 只跑 GPT，`gpt-` 這三個字每一顆都一樣（2026-09-12 使用者：「codex 肯定是 gpt-xxx
  // 所以去掉 gpt-」）。留下來的是真正在區分的那半截（`gpt-6-astra` → `6-astra`），完整 id
  // 仍在 tooltip 裡。
  if (kind === 'codex') return model.replace(/^gpt-/, '')
  // agy 只跑 Gemini，slug 尾巴的 high／medium／low 其實是強度（2026-10-17 使用者：「只要 3.8-flash 就好而 high 是 effort」），
  // 強度由 [`agyEffort`] 拆出來放在強度那格。
  if (kind === 'agy') return model.replace(/^gemini-/, '').replace(AGY_EFFORT_SUFFIX, '')
  if (kind !== 'claude') return model
  const m = /^claude-(fable|opus|sonnet|haiku)(?:-\d.*)?$/.exec(model)
  return m ? m[1] : model
}


const AGY_EFFORT_SUFFIX = /-(low|medium|high)$/

/** agy slug 尾巴的強度（`gemini-3.8-flash-high` → `high`）；不是 agy 或沒有強度尾巴回 `null`。 */
export function agyEffort(kind: string, model: string | null): string | null {
  if (kind !== 'agy' || !model) return null
  return AGY_EFFORT_SUFFIX.exec(model)?.[1] ?? null
}
