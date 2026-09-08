/**
 * `claude-fable-5-1` → `fable`、`claude-opus-5` → `opus`：子 agent 從 argv / statusLine 讀回來的是
 * 完整 model id，跟旁邊使用者自己選的 `opus` 擺在一起就像另一個模型；側欄那格只放別名。
 * 認不得的原樣顯示，完整 id 留在 tooltip。
 */
export function shortModel(kind: string, model: string | null): string | null {
  if (!model) return null
  if (kind !== 'claude') return model
  const m = /^claude-(fable|opus|sonnet|haiku)(?:-\d.*)?$/.exec(model)
  return m ? m[1] : model
}

