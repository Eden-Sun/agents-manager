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
