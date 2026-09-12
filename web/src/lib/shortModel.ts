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
  if (kind !== 'claude') return model
  const m = /^claude-(fable|opus|sonnet|haiku)(?:-\d.*)?$/.exec(model)
  return m ? m[1] : model
}

