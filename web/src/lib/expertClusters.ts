import type { Lamp } from '../api/types'
import { t } from './i18n'

export type ExpertClusterItem<T extends { cluster?: string | null }> =
  | { kind: 'single'; item: T }
  | { kind: 'cluster'; name: string; members: T[] }

export function groupClusterItems<T extends { cluster?: string | null }>(items: readonly T[]): ExpertClusterItem<T>[] {
  const count = new Map<string, number>()
  for (const b of items) if (b.cluster) count.set(b.cluster, (count.get(b.cluster) ?? 0) + 1)
  const clusters = new Map<string, T[]>()
  const singles: T[] = []
  for (const b of items) {
    const name = b.cluster
    if (!name || (count.get(name) ?? 0) < 2) {
      singles.push(b)
      continue
    }
    const list = clusters.get(name)
    if (list) list.push(b)
    else clusters.set(name, [b])
  }
  return [
    ...[...clusters].map(([name, members]): ExpertClusterItem<T> => ({ kind: 'cluster', name, members })),
    ...singles.map((item): ExpertClusterItem<T> => ({ kind: 'single', item })),
  ]
}

export type ClusterStat = 'working' | 'idle' | 'blocked' | 'starting' | 'stopped' | 'disconnected' | 'unknown'

const CLUSTER_STAT_ORDER: readonly ClusterStat[] = ['working', 'idle', 'blocked', 'starting', 'stopped', 'disconnected', 'unknown']

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

export function clusterStatText(lamps: readonly Lamp[]): string {
  return clusterStatCounts(lamps)
    .map(({ stat, n }) => clusterStatLabel(stat, n))
    .join(' · ')
}

export function clusterStatCounts(lamps: readonly Lamp[]): { stat: ClusterStat; n: number }[] {
  const counts = new Map<ClusterStat, number>()
  for (const lamp of lamps) {
    const stat = clusterStatOf(lamp)
    counts.set(stat, (counts.get(stat) ?? 0) + 1)
  }
  return CLUSTER_STAT_ORDER.flatMap((stat) => {
    const n = counts.get(stat) ?? 0
    return n > 0 ? [{ stat, n }] : []
  })
}
