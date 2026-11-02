import type { Lamp } from '../api/types'
import { t } from './i18n'

/** 同群集的成員（cluster 相同、≥2 顆）收成一列；其餘維持單列。保留原順序，群集列落在第一個成員的位置。 */
export type ExpertClusterItem<T extends { cluster?: string | null }> =
  | { kind: 'single'; item: T }
  | { kind: 'cluster'; name: string; members: T[] }

export function groupClusterItems<T extends { cluster?: string | null }>(items: readonly T[]): ExpertClusterItem<T>[] {
  const count = new Map<string, number>()
  for (const b of items) if (b.cluster) count.set(b.cluster, (count.get(b.cluster) ?? 0) + 1)
  const out: ExpertClusterItem<T>[] = []
  const seen = new Map<string, T[]>()
  for (const b of items) {
    const name = b.cluster
    if (!name || (count.get(name) ?? 0) < 2) {
      out.push({ kind: 'single', item: b })
      continue
    }
    const list = seen.get(name)
    if (list) {
      list.push(b)
    } else {
      const members = [b]
      seen.set(name, members)
      out.push({ kind: 'cluster', name, members })
    }
  }
  return out
}

/** 群集列第三行的狀態分類（每顆成員的燈號只歸一類；offline 與 stopping 都算「停止」）。 */
export type ClusterStat = 'working' | 'idle' | 'blocked' | 'starting' | 'stopped' | 'disconnected' | 'unknown'

/** 列出的順序；數字為 0 的項目不列。 */
const CLUSTER_STAT_ORDER: readonly ClusterStat[] = ['working', 'idle', 'blocked', 'starting', 'stopped', 'disconnected', 'unknown']

/** 一種狀態的文字（字面 key，讓 i18n 掃描抓得到插值）。 */
function clusterStatLabel(stat: ClusterStat, n: number): string {
  switch (stat) {
    case 'working':
      return t('{n} 執行中', { n })
    case 'idle':
      return t('{n} 閒置', { n })
    case 'blocked':
      return t('{n} 等待回應', { n })
    case 'starting':
      return t('{n} 啟動中', { n })
    case 'stopped':
      return t('{n} 停止', { n })
    case 'disconnected':
      return t('{n} 連線中斷', { n })
    case 'unknown':
      return t('{n} 狀態未知', { n })
  }
}

/** 把 botLamp 的燈號對到群集列的狀態分類。 */
export function clusterStatOf(lamp: Lamp): ClusterStat {
  switch (lamp) {
    case 'offline':
    case 'stopping':
      return 'stopped'
    case 'working':
    case 'idle':
    case 'blocked':
    case 'starting':
    case 'disconnected':
    case 'unknown':
      return lamp
  }
}

/**
 * 群集列第三行：每種狀態都列出、數字為 0 的省略，用 ` · ` 串起來，所以各項加總等於成員數。
 * 例：`1 執行中 · 1 閒置 · 2 停止`。
 */
export function clusterStatText(lamps: readonly Lamp[]): string {
  const counts = new Map<ClusterStat, number>()
  for (const lamp of lamps) {
    const stat = clusterStatOf(lamp)
    counts.set(stat, (counts.get(stat) ?? 0) + 1)
  }
  return CLUSTER_STAT_ORDER.flatMap((stat) => {
    const n = counts.get(stat) ?? 0
    return n > 0 ? [clusterStatLabel(stat, n)] : []
  }).join(' · ')
}
