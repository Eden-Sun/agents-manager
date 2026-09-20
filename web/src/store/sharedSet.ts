/**
 * 收合狀態（`am.collapsedProjects`／`am.collapsedChildren`）存在所有分頁共用的 localStorage。
 * 整份覆寫會洗掉別的分頁剛收合／展開的項目（同型於 #364 的草稿）：
 * 只把相對 prev 有增減的 id 套到磁碟現有那份。
 */
export function persistSetDiff(key: string, prev: ReadonlySet<string>, next: ReadonlySet<string>): void {
  let disk = new Set<string>()
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(key) ?? '[]')
    if (Array.isArray(parsed)) disk = new Set(parsed.filter((v): v is string => typeof v === 'string'))
  } catch {
    /* 壞掉的值當作空的，之後被覆寫成合法內容 */
  }
  for (const id of prev) if (!next.has(id)) disk.delete(id)
  for (const id of next) if (!prev.has(id)) disk.add(id)
  localStorage.setItem(key, JSON.stringify([...disk]))
}
