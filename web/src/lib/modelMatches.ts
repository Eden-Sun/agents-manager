/**
 * 剝除模型字串的 context 視窗後綴（如 `[1m]`），去頭尾空白並轉小寫。
 */
export function stripModelContext(model: string | null | undefined): string {
  if (!model) return ''
  return model.split('[')[0].trim().toLowerCase()
}

/**
 * 存的模型對到哪一顆按鈕，或 running 與 configured 模型是否等價：
 * 1. 忽略 context 視窗後綴（如 `[1m]`）與大小寫；
 * 2. 完全相等；
 * 3. claude 系列別名（`opus`/`sonnet`/`haiku`/`fable`）與其完整版號（`claude-<alias>-<version>`）互相等價。
 * daemon 會把 `opus` 存成完整版號（#400），且 500k context 會加上 `[1m]` 變體。
 */
export function modelMatches(a: string | null | undefined, b: string | null | undefined): boolean {
  const normA = stripModelContext(a)
  const normB = stripModelContext(b)
  if (!normA || !normB) return false
  if (normA === normB) return true
  const isFamily = (alias: string, full: string) =>
    !alias.includes('-') && full.startsWith(`claude-${alias}-`)
  return isFamily(normA, normB) || isFamily(normB, normA)
}
