import type { Bot } from '../api/types'
import { roleShortLabel, roleTitle } from '../lib/expertSections'
import { t, useT } from '../lib/i18n'
import './expertBadge.css'

export function ExpertBadge() {
  useT()
  return (
    <span className="project-expert-tag" title={t('專家模式專案')}>
      {t('專家')}
    </span>
  )
}

export function ExpertRoleTag({
  role,
  rolePending,
}: {
  role: Bot['role']
  rolePending?: boolean
}) {
  useT()
  const short = roleShortLabel(role)
  if (!short) return null

  const title = roleTitle(role)
  const fullTitle = rolePending ? `${title} · ${t('待套用（需重啟）')}` : title

  return (
    <span className={`bot-role-tag role-${role ?? 'none'}${rolePending ? ' pending' : ''}`} title={fullTitle}>
      <span className="bot-role-text">{short}</span>
      {rolePending ? <span className="bot-role-dot" aria-hidden="true" /> : null}
    </span>
  )
}

export function ExpertSectionHeader({
  title,
  count,
}: {
  title: string
  count: number
}) {
  return (
    <div className="expert-section-head" role="presentation" aria-hidden="true">
      <span className="expert-section-title">{title}</span>
      <span className="expert-section-count">{count}</span>
    </div>
  )
}
