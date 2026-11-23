import { useState } from 'react'
import type { Bot, Lamp } from '../api/types'
import { clusterStatCounts, clusterStatText } from '../lib/expertClusters'
import { roleShortLabel, roleTitle } from '../lib/expertSections'
import { t, useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { NoDispatchIcon } from './Icons'
import './expertBadge.css'

export function ExpertBadge() {
  useT()
  return (
    <span className="project-expert-tag" title={t('專家模式專案')}>
      {t('專家模式').replace(/模式$| mode$/, '')}
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
    <span className="bot-nodispatch bot-manager-disabled-tag" title={t('已停派：管理者不能派工、啟動或重啟它（Bot 本身沒停）。')}>
      <NoDispatchIcon />
      <span className="bot-nodispatch-label">{t('停派')}</span>
    </span>
  )
}

export function ExpertSectionHeader({
  title,
  abbr,
  collapsible = false,
  open = true,
  onToggle,
  lamps,
}: {
  title: string
  abbr?: string
  collapsible?: boolean
  open?: boolean
  onToggle?: () => void
  lamps?: readonly Lamp[]
}) {
  if (!collapsible) {
    return (
      <div className="expert-section-head" role="presentation" aria-hidden="true">
        <span className="expert-section-title">{title}</span>
        {abbr && abbr !== title ? <span className="expert-section-abbr">{abbr}</span> : null}
      </div>
    )
  }
  const counts = clusterStatCounts(lamps ?? [])
  return (
    <button type="button" className="expert-section-head expert-section-toggle" aria-expanded={open} onClick={onToggle} title={clusterStatText(lamps ?? [])}>
      <span className="expert-section-caret" aria-hidden="true">{open ? '▾' : '▸'}</span>
      <span className="expert-section-title">{title}</span>
      {abbr && abbr !== title ? <span className="expert-section-abbr">{abbr}</span> : null}
      <span className="expert-section-stat">
        {counts.map(({ stat, n }) => (
          <span key={stat} className="bot-cluster-stat-item">
            <span className={`lamp lamp-${stat === 'stopped' ? 'offline' : stat} bot-cluster-dot`} aria-hidden="true" />
            {n}
          </span>
        ))}
      </span>
    </button>
  )
}

export function UnsetRoleBadge({ botId }: { botId: string }) {
  useT()
  const [open, setOpen] = useState(false)
  const busy = useStore((s) => Boolean(s.busy[`patch:${botId}`]))
  const choose = (role: 'executor' | 'advisor' | 'scheduler') => {
    setOpen(false)
    void useStore.getState().patchBot(botId, { role })
  }
  const stop = (e: { stopPropagation: () => void }) => e.stopPropagation()
  return (
    <span className="bot-unset-role-wrap" onClick={stop} onMouseDown={stop} onKeyDown={stop}>
      <button type="button" className="bot-unset-role" title={t('這顆 Bot 還沒有角色：點了選角色')} aria-haspopup="menu" aria-expanded={open} disabled={busy} onClick={() => setOpen((v) => !v)}>
        {t('未設角色')}
      </button>
      {open ? (
        <span className="bot-unset-role-menu" role="menu">
          {(['executor', 'advisor', 'scheduler'] as const).map((r) => (
            <button key={r} type="button" role="menuitem" onClick={() => choose(r)}>{roleTitle(r)}</button>
          ))}
        </span>
      ) : null}
    </span>
  )
}
