export function persistSetDiff(key: string, prev: ReadonlySet<string>, next: ReadonlySet<string>): void {
  let disk = new Set<string>()
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(key) ?? '[]')
    if (Array.isArray(parsed)) disk = new Set(parsed.filter((v): v is string => typeof v === 'string'))
  } catch {
  }
  for (const id of prev) if (!next.has(id)) disk.delete(id)
  for (const id of next) if (!prev.has(id)) disk.add(id)
  localStorage.setItem(key, JSON.stringify([...disk]))
}
