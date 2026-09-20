/**
 * 預覽面板「本機已開的 dev server」清單的純函式（issue #253 v6）：短路徑、排序、收合、同目錄標記。
 * 元件在 `components/PreviewPanel.tsx`。
 */
import type { PreviewOther } from '../api/preview'

function segs(p: string): string[] {
  return p.split('/').filter(Boolean)
}

/**
 * 短路徑：在專案根底下＝相對於根（根本身＝根的最後一段）；不在專案底下＝最後兩段。
 * 完整路徑由呼叫端放 `title`。
 */
export function shortPath(dir: string, root: string | null): string {
  const d = segs(dir)
  const r = root ? segs(root) : []
  if (r.length > 0 && r.length <= d.length && r.every((s, i) => s === d[i])) {
    return r.length === d.length ? r[r.length - 1] : d.slice(r.length).join('/')
  }
  return d.slice(-2).join('/') || dir
}

/** repo 標記跟短路徑重複（短路徑裡已有那一段）就不要再印。 */
export function showRepo(repo: string | null, short: string): boolean {
  return Boolean(repo) && !segs(short).includes(repo as string)
}

export interface ListRow {
  o: PreviewOther
  short: string
  /** 同一組裡還有別的 port 在同一個目錄：多顆標「同目錄」。 */
  sharedDir: boolean
}

/**
 * 一組內的排序：認得出框架的（kind 非 unknown）在前，各自照 port；同目錄的排在一起（以該目錄最小的 port 為序）。
 * `known`／`unknown` 分開回，unknown 預設收起。
 */
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
