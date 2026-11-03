/**
 * 登入協助正在看的登入 pane。CLI 結束、daemon 收掉 pane 之後，`HostShellPanel` 讀到 404 不能照一般 shell 那樣把面板拆掉：
 * 拆掉的話協助條（`ClaudeLoginAssist`）連同「已登入／仍未登入」的結果一起消失，使用者只剩一個關掉的面板（#登入後沒有結果，2026-11-03）。
 * 只記在記憶體、只看 (host, pane)；協助條卸載時放開。
 */
const held = new Set<string>()

const keyOf = (host: string, paneId: string) => `${host || 'local'}/${paneId}`

/** 開始看這顆登入 pane；回傳放開的函式（協助條卸載時呼叫）。 */
export function holdLoginPane(host: string, paneId: string): () => void {
  const k = keyOf(host, paneId)
  held.add(k)
  return () => {
    held.delete(k)
  }
}

export function isHeldLoginPane(host: string, paneId: string): boolean {
  return held.has(keyOf(host, paneId))
}
