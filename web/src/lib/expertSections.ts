import type { Bot } from '../api/types'
import { t } from './i18n'

export type ExpertRole = 'manager' | 'specialist' | 'advisor' | 'executor' | 'scheduler'

export interface ExpertSection<T extends { role?: Bot['role'] }> {
  role: ExpertRole
  title: string
  shortLabel: string
  abbr: string
  items: T[]
}

export const EXPERT_ROLE_ORDER: readonly ExpertRole[] = ['manager', 'specialist', 'advisor', 'executor', 'scheduler'] as const

const ROLE_ABBR: Record<ExpertRole, string> = { manager: 'Mgr', specialist: 'Spc', advisor: 'Adv', executor: 'Wkr', scheduler: '' }

export function roleAbbr(role: ExpertRole): string {
  return ROLE_ABBR[role]
}

export function roleToExpertSection(role: Bot['role'], scheduled = false): ExpertRole {
  if (role === 'manager') return 'manager'
  if (role === 'specialist') return 'specialist'
  if (role === 'advisor') return 'advisor'
  if (role === 'executor') return 'executor'
  if (role === 'scheduler') return 'scheduler'
  return scheduled ? 'scheduler' : 'executor'
}

export function roleShortLabel(role: Bot['role'] | ExpertRole): string {
  if (role === 'manager') return t('管')
  if (role === 'specialist') return t('專')
  if (role === 'advisor') return t('顧')
  if (role === 'executor') return t('執')
  return ''
}

export function roleTitle(role: Bot['role'] | ExpertRole): string {
  if (role === 'manager') return t('管理者')
  if (role === 'specialist') return t('專家')
  if (role === 'advisor') return t('顧問')
  if (role === 'executor') return t('執行者')
  if (role === 'scheduler') return t('排程專用')
  return ''
}

export function groupExpertSections<T extends { id: string; role?: Bot['role']; scheduled?: boolean }>(items: readonly T[], deputyBotId?: string | null): ExpertSection<T>[] {
  const buckets: Record<ExpertRole, T[]> = {
    manager: [],
    specialist: [],
    advisor: [],
    executor: [],
    scheduler: [],
  }

  let deputy: T | undefined
  for (const item of items) {
    if (deputyBotId && item.id === deputyBotId) {
      deputy = item
      continue
    }
    const bucket = roleToExpertSection(item.role, item.scheduled)
    buckets[bucket].push(item)
  }
  if (deputy) buckets.manager.push(deputy)

  const sections: ExpertSection<T>[] = []
  for (const r of EXPERT_ROLE_ORDER) {
    const group = buckets[r]
    if (group.length > 0) {
      sections.push({
        role: r,
        title: roleTitle(r),
        shortLabel: roleShortLabel(r),
        abbr: roleAbbr(r),
        items: group,
      })
    }
  }

  return sections
}

export function canReorderInExpertMode(source: { role?: Bot['role']; scheduled?: boolean }, target: { role?: Bot['role']; scheduled?: boolean }): boolean {
  return roleToExpertSection(source.role, source.scheduled) === roleToExpertSection(target.role, target.scheduled)
}
