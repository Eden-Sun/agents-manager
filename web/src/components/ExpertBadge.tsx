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

/** 專家（specialist）負責的軌道：名字旁的灰色小字。 */
export function ExpertTrackTag({ track }: { track: string }) {
  return <span className="bot-track-tag" title={t('負責任務：{track}', { track })}>{track}</span>
}

export function ExpertDisabledMark() {
  useT()
  return (
    <span className="bot-manager-disabled-tag" title={t('已停用：管理者不能使用此成員')}>
      {t('停用')}
    </span>
  )
}

export function ExpertSectionHeader({ title }: { title: string }) {
  return (
    <div className="expert-section-head" role="presentation" aria-hidden="true">
      <span className="expert-section-title">{title}</span>
    </div>
  )
}
