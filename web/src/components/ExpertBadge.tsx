import { useState } from 'react'
import type { Bot } from '../api/types'
import { roleShortLabel, roleTitle } from '../lib/expertSections'
import { t, useT } from '../lib/i18n'
import { useStore } from '../store/store'
import './expertBadge.css'

export function ExpertBadge() {
  useT()
  // 專案徽章是「專家模式」，不是角色「專家」（英文 Specialist）：同一個中文字，英文要分開。
  return (
    <span className="project-expert-tag" title={t('專家模式專案')}>
      {t('專家模式').replace(/模式$| mode$/, '')}
    </span>
  )
}

/** 專家模式側欄列上的管理者可用勾選框：直接勾、直接取消（使用者 2026-11-04：不用再開 ⋯ 選單）。 */
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
  const label = t('允許管理者派工')
  const title = effectiveChecked ? t('關閉後管理者不能派工給它、不能啟動或重啟它') : t('開啟後管理者可派工、啟動或重啟此成員')

  return (
    <label
      className="bot-manager-use-wrap"
      onClick={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <input
        type="checkbox"
        className="bot-manager-use-checkbox"
        aria-label={label}
        title={title}
        checked={effectiveChecked}
        disabled={disabled || isPatching}
        onChange={(e) => {
          const next = e.target.checked
          setLocalOptimistic(next)
          void useStore
            .getState()
            .patchBot(botId, { manager_use: next })
            .finally(() => setLocalOptimistic(null))
        }}
      />
    </label>
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

/** 副管理者：成員列第二行的小標。 */
export function ExpertDeputyTag({ afterSec }: { afterSec?: number | null }) {
  useT()
  const title = afterSec
    ? t('副管理者：管理者回合中超過 {sec} 秒時，代收排隊中的請求', { sec: afterSec })
    : t('副管理者：管理者回合中太久時，代收排隊中的請求')
  return <span className="bot-deputy-tag" title={title}>{t('副')}</span>
}

/** 成員列上的「停派」：管理者不能派工、啟動或重啟它（Bot 本身沒停）。點一下送 `manager_use: true` 恢復。 */
export function ExpertDisabledMark({ botId }: { botId: string }) {
  useT()
  const busy = useStore((s) => Boolean(s.busy[`patch:${botId}`]))
  const stop = (e: { stopPropagation: () => void }) => e.stopPropagation()
  return (
    <button
      type="button"
      className="bot-manager-disabled-tag"
      title={t('已停派：管理者不能派工、啟動或重啟它（Bot 本身沒停）。點一下恢復')}
      disabled={busy}
      onClick={(e) => {
        stop(e)
        void useStore.getState().patchBot(botId, { manager_use: true })
      }}
      onMouseDown={stop}
      onKeyDown={stop}
    >
      {t('停派')}
    </button>
  )
}

export function ExpertSectionHeader({ title, abbr }: { title: string; abbr?: string }) {
  return (
    <div className="expert-section-head" role="presentation" aria-hidden="true">
      <span className="expert-section-title">{title}</span>
      {abbr && abbr !== title ? <span className="expert-section-abbr">{abbr}</span> : null}
    </div>
  )
}

/**
 * 專家模式下 role 為空的頂層 bot（歸在執行者區）的小徽章「未設角色」：點了直接選角色（執行者／顧問／排程專用），
 * 走既有 `PATCH /api/bots/{id}` 的 role（權限同 bot 設定；管理者、專家要在 bot 設定裡指派，因為專家需要 track）。
 */
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
