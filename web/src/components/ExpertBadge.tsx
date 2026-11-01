import { useState } from 'react'
import type { Bot } from '../api/types'
import { roleShortLabel, roleTitle } from '../lib/expertSections'
import { t, useT } from '../lib/i18n'
import { useStore } from '../store/store'
import './expertBadge.css'

export function ExpertBadge() {
  useT()
  return (
    <span className="project-expert-tag" title={t('專家模式專案')}>
      {t('專家')}
    </span>
  )
}

/** 專家模式側欄列上的管理者可用勾選框 */
export function ManagerUseCheckbox({
  botId,
  managerUse,
  disabled,
}: {
  botId: string
  managerUse: boolean
  disabled?: boolean
}) {
  useT()
  const isPatching = useStore((s) => Boolean(s.busy[`patch:${botId}`]))
  const [localOptimistic, setLocalOptimistic] = useState<boolean | null>(null)

  const effectiveChecked = localOptimistic !== null ? localOptimistic : managerUse
  const title = t('管理者可用：開啟後管理者可派工、啟動或重啟此成員')

  return (
    <span
      className="bot-manager-use-wrap"
      onClick={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <input
        type="checkbox"
        className="bot-manager-use-checkbox"
        aria-label={title}
        title={title}
        checked={effectiveChecked}
        disabled={disabled || isPatching}
        onChange={(e) => {
          const nextVal = e.target.checked
          setLocalOptimistic(nextVal)
          const patch = useStore.getState().patchBot
          void patch(botId, { manager_use: nextVal }).then((res) => {
            if (!res) {
              setLocalOptimistic(null)
            } else {
              setLocalOptimistic(null)
            }
          }).catch(() => {
            setLocalOptimistic(null)
          })
        }}
      />
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

/** 副管理者：成員列第二行的小標。 */
export function ExpertDeputyTag({ afterSec }: { afterSec?: number | null }) {
  useT()
  const title = afterSec
    ? t('副管理者：管理者回合中超過 {sec} 秒時，代收排隊中的請求', { sec: afterSec })
    : t('副管理者：管理者回合中太久時，代收排隊中的請求')
  return <span className="bot-deputy-tag" title={title}>{t('副')}</span>
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
