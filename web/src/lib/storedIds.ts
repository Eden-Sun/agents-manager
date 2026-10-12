/**
 * 已刪除的 bot／專案留在 localStorage 的鍵：收合狀態、預覽欄展開、上次選的種類。`pruneDead` 拿快照上的名單清一次，
 * 不然只增不減。只動這幾把鍵（不認得的一律不碰）；每一把各自包 try/catch——隱私模式下讀寫會丟例外，清不掉就算了。
 * 鍵名要跟寫入端一致：Sidebar（`am.collapsedChildren`、`am.collapsedProjects`、`am:lastKind:<pid>`）、
 * `store/previewLayout.ts`（`am.previewCol.openBots`）。
 */
const ID_LISTS: { key: string; kind: 'bot' | 'project' }[] = [
  { key: 'am.collapsedChildren', kind: 'bot' },
  { key: 'am.collapsedProjects', kind: 'project' },
]
const ID_MAPS: { key: string; kind: 'bot' | 'project' }[] = [{ key: 'am.previewCol.openBots', kind: 'bot' }]
const LAST_KIND_PREFIX = 'am:lastKind:'

function attempt(run: () => void): void {
  try {
    run()
  } catch {
    /* 讀寫不了就算了：這些只是版面偏好 */
  }
}

export function pruneStoredIds(live: { botIds: readonly string[]; projectIds: readonly string[] }): void {
  const alive = {
    bot: new Set(live.botIds),
    project: new Set(live.projectIds),
  }
  for (const { key, kind } of ID_LISTS) {
    attempt(() => {
      const raw = localStorage.getItem(key)
      if (!raw) return
      const parsed: unknown = JSON.parse(raw)
      if (!Array.isArray(parsed)) return
      const kept = parsed.filter((id): id is string => typeof id === 'string' && alive[kind].has(id))
      if (kept.length !== parsed.length) localStorage.setItem(key, JSON.stringify(kept))
    })
  }
  for (const { key, kind } of ID_MAPS) {
    attempt(() => {
      const raw = localStorage.getItem(key)
      if (!raw) return
      const parsed: unknown = JSON.parse(raw)
      if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) return
      const entries = Object.entries(parsed)
      const kept = Object.fromEntries(entries.filter(([id]) => alive[kind].has(id)))
      if (Object.keys(kept).length !== entries.length) localStorage.setItem(key, JSON.stringify(kept))
    })
  }
  attempt(() => {
    const stale: string[] = []
    for (let i = 0; i < localStorage.length; i += 1) {
      const k = localStorage.key(i)
      if (k?.startsWith(LAST_KIND_PREFIX) && !alive.project.has(k.slice(LAST_KIND_PREFIX.length))) stale.push(k)
    }
    for (const k of stale) localStorage.removeItem(k)
  })
}
