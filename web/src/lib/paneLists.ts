/**
 * 側欄與專案頁共用的 pane 清單（SPEC §6.5e）。
 *
 * 以前兩份各管各的：側欄只在展開專案時讀一次、專案頁自己 `useState` 每 30 秒讀、關掉只 reload 自己，
 * 於是專案頁關掉的 pane 側欄還在，點下去面板閃一下就消失；而且側欄把「沒歸屬」的 pane 掛在同一台
 * **每個**專案底下（第二輪 review M3／M4）。現在 store 只有一份，從 `GET /api/panes` 依 `project_id` 分；
 * 對不到專案的放側欄底部「開 shell」旁那一組。
 */
import type { ProjectPane } from '../api'

/** 依 `project_id` 分組；對不到專案的（`null`）不掛在任何專案底下。 */
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

/**
 * 側欄底部那一組。scratch 由 daemon 標（`scratch: true`），前端不自己重算——兩邊各算一次遲早對不上，
 * 會把 GC 準備關掉的那顆當成 scratch。daemon 沒給這欄（舊版）就全部列、一個都不標。
 */
export function unownedRows(list: readonly ProjectPane[]): { pane: ProjectPane; tag: UnownedTag }[] {
  const marked = list.some((p) => typeof p.scratch === 'boolean')
  const rows = list.map((pane) => ({ pane, tag: (marked ? (pane.scratch ? 'scratch' : 'extra') : null) as UnownedTag }))
  // scratch 固定排第一列。
  return rows.sort((a, b) => Number(b.tag === 'scratch') - Number(a.tag === 'scratch'))
}

export interface PaneLists {
  sidePanes: Record<string, ProjectPane[]>
  unownedPanes: ProjectPane[]
}

/** 一顆 pane 不在了（關掉、404）：兩份清單一起拿掉，不等下一輪輪詢。 */
export function withoutPane(lists: PaneLists, host: string, paneId: string): PaneLists {
  const gone = (p: ProjectPane) => p.host === host && p.pane_id === paneId
  const sidePanes: Record<string, ProjectPane[]> = {}
  for (const [pid, list] of Object.entries(lists.sidePanes)) {
    const kept = list.filter((p) => !gone(p))
    if (kept.length > 0) sidePanes[pid] = kept
  }
  return { sidePanes, unownedPanes: lists.unownedPanes.filter((p) => !gone(p)) }
}

/** 同一顆 pane 換成 daemon 剛回的那一列（例如關閉被 409 擋下、附上的最新 kind／port）。 */
export function withPane(lists: PaneLists, fresh: ProjectPane): PaneLists {
  const same = (p: ProjectPane) => p.host === fresh.host && p.pane_id === fresh.pane_id
  const sidePanes: Record<string, ProjectPane[]> = {}
  for (const [pid, list] of Object.entries(lists.sidePanes)) sidePanes[pid] = list.map((p) => (same(p) ? fresh : p))
  return { sidePanes, unownedPanes: lists.unownedPanes.map((p) => (same(p) ? fresh : p)) }
}
