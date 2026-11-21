import type { ProjectPane } from '../api'

export function groupByProject(all: readonly ProjectPane[]): Record<string, ProjectPane[]> {
  const out: Record<string, ProjectPane[]> = {}
  for (const p of all) {
    if (!p.project_id) continue
    const list = out[p.project_id] ?? []
    list.push(p)
    out[p.project_id] = list
  }
  return out
}

export type UnownedTag = 'scratch' | 'extra' | null

export function unownedRows(list: readonly ProjectPane[]): { pane: ProjectPane; tag: UnownedTag }[] {
  const marked = list.some((p) => typeof p.scratch === 'boolean')
  const rows = list.map((pane) => ({ pane, tag: (marked ? (pane.scratch ? 'scratch' : 'extra') : null) as UnownedTag }))
  return rows.sort((a, b) => Number(b.tag === 'scratch') - Number(a.tag === 'scratch'))
}

export interface PaneLists {
  sidePanes: Record<string, ProjectPane[]>
  unownedPanes: ProjectPane[]
}

export function withoutPane(lists: PaneLists, host: string, paneId: string): PaneLists {
  const gone = (p: ProjectPane) => p.host === host && p.pane_id === paneId
  const sidePanes: Record<string, ProjectPane[]> = {}
  for (const [pid, list] of Object.entries(lists.sidePanes)) {
    const kept = list.filter((p) => !gone(p))
    if (kept.length > 0) sidePanes[pid] = kept
  }
  return { sidePanes, unownedPanes: lists.unownedPanes.filter((p) => !gone(p)) }
}

export function withPane(lists: PaneLists, fresh: ProjectPane): PaneLists {
  const same = (p: ProjectPane) => p.host === fresh.host && p.pane_id === fresh.pane_id
  const sidePanes: Record<string, ProjectPane[]> = {}
  for (const [pid, list] of Object.entries(lists.sidePanes)) sidePanes[pid] = list.map((p) => (same(p) ? fresh : p))
  return { sidePanes, unownedPanes: lists.unownedPanes.map((p) => (same(p) ? fresh : p)) }
}
