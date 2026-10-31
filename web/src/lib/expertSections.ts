import type { Bot } from '../api/types'
import { t } from './i18n'

export type ExpertRole = 'manager' | 'advisor' | 'executor' | 'unassigned'

export interface ExpertSection<T extends { role?: Bot['role'] }> {
  role: ExpertRole
  title: string
  shortLabel: string
  items: T[]
}

export const EXPERT_ROLE_ORDER: readonly ExpertRole[] = ['manager', 'advisor', 'executor', 'unassigned'] as const

export function roleToExpertSection(role: Bot['role']): ExpertRole {
  if (role === 'manager') return 'manager'
  if (role === 'advisor') return 'advisor'
  if (role === 'executor') return 'executor'
  return 'unassigned'
}

export function roleShortLabel(role: Bot['role'] | ExpertRole): string {
  if (role === 'manager') return t('管')
  if (role === 'advisor') return t('顧')
  if (role === 'executor') return t('執')
  return ''
}

export function roleTitle(role: Bot['role'] | ExpertRole): string {
  if (role === 'manager') return t('管理者')
  if (role === 'advisor') return t('顧問')
  if (role === 'executor') return t('執行者')
  return t('未分配')
}

/**
 * 在專家模式下，依固定順序（管理者 -> 顧問 -> 執行者 -> 未分配）將頂層項目分段；
 * 沒有項目的分段自動略過不畫。
 */
export function groupExpertSections<T extends { role?: Bot['role'] }>(items: readonly T[]): ExpertSection<T>[] {
  const buckets: Record<ExpertRole, T[]> = {
    manager: [],
    advisor: [],
    executor: [],
    unassigned: [],
  }

  for (const item of items) {
    const bucket = roleToExpertSection(item.role)
    buckets[bucket].push(item)
  }

  const sections: ExpertSection<T>[] = []
  for (const r of EXPERT_ROLE_ORDER) {
    const group = buckets[r]
    if (group.length > 0) {
      sections.push({
        role: r,
        title: roleTitle(r),
        shortLabel: roleShortLabel(r),
        items: group,
      })
    }
  }

  return sections
}

/**
 * 專家模式下的拖曳與移動限制：只能在同一角色段落內重排，不可跨段。
 */
export function canReorderInExpertMode(sourceRole: Bot['role'], targetRole: Bot['role']): boolean {
  return roleToExpertSection(sourceRole) === roleToExpertSection(targetRole)
}
