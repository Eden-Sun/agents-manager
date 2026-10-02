/**
 * 存的模型對到哪一顆按鈕：完全相等，或 claude 的完整版號對到它的系列別名（`claude-opus-5-5` → `opus`）。
 * daemon 會把 `opus` 存成完整版號（#400），以前比對要完全相等，Opus 的 bot 開設定時一顆都沒亮（2026-10-02 使用者）。
 */
export function modelMatches(chipId: string, stored: string | null): boolean {
  if (!stored) return false
  if (chipId === stored) return true
  return !chipId.includes('-') && stored.startsWith(`claude-${chipId}-`)
}
