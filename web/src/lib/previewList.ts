import type { PreviewOther } from '../api/preview'

function segs(p: string): string[] {
  return p.split('/').filter(Boolean)
}

export function shortPath(dir: string, root: string | null): string {
  const d = segs(dir)
  const r = root ? segs(root) : []
  if (r.length > 0 && r.length <= d.length && r.every((s, i) => s === d[i])) {
    return r.length === d.length ? r[r.length - 1] : d.slice(r.length).join('/')
  }
  return d.slice(-2).join('/') || dir
}

export function showRepo(repo: string | null, short: string): boolean {
  return Boolean(repo) && !segs(short).includes(repo as string)
}

export interface ListRow {
  o: PreviewOther
  short: string
  sharedDir: boolean
}

export function orderRows(items: PreviewOther[], root: string | null): { known: ListRow[]; unknown: ListRow[] } {
  const minPort = new Map<string, number>()
  const count = new Map<string, number>()
  for (const o of items) {
    minPort.set(o.dir, Math.min(minPort.get(o.dir) ?? Infinity, o.port))
    count.set(o.dir, (count.get(o.dir) ?? 0) + 1)
  }
  const rows = (list: PreviewOther[]): ListRow[] =>
    [...list]
      .sort((a, b) => (minPort.get(a.dir)! - minPort.get(b.dir)!) || a.port - b.port)
      .map((o) => ({ o, short: shortPath(o.dir, root), sharedDir: (count.get(o.dir) ?? 0) > 1 }))
  return {
    known: rows(items.filter((o) => o.kind.toLowerCase() !== 'unknown')),
    unknown: rows(items.filter((o) => o.kind.toLowerCase() === 'unknown')),
  }
}
