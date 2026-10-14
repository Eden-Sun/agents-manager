/** 曾經解析到的代發來源名字（bot id → 名字）。來源 bot 收掉、清單移除後，訊息上仍顯示名字而不是 ULID。
 * 模組層、只記在這個分頁裡（重新載入後靠 daemon 補的 `relay_from_name` 兜底）。 */
const seen = new Map<string, string>()

export function rememberRelayName(id: string, name: string): void {
  if (id && name) seen.set(id, name)
}

export function recallRelayName(id: string): string | undefined {
  return seen.get(id)
}
