/** 目錄路徑的最後一段（`/a/b/` → `b`）；根目錄回空字串。 */
export function dirLabel(path: string): string {
  return path.split('/').filter(Boolean).pop() ?? ''
}

/**
 * 挑完目錄後標籤欄該是什麼：空的、或還是上一次自動帶入的那個（使用者沒改過）就換成新目錄名；使用者自己打的不動。
 * `lastAuto` 是上一次自動帶入的值（沒帶過就是空字串）。
 */
export function labelAfterPick(current: string, lastAuto: string, picked: string): string {
  return !current.trim() || current === lastAuto ? dirLabel(picked) : current
}
