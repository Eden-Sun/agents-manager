import { useEffect, useState, type KeyboardEvent } from 'react'
import { useStore } from '../store/store'
import {
  ENV_TABS,
  getRememberedTab,
  rememberTab,
  resolveInitialEnvTab,
  type EnvTabBadges,
  type EnvTabId,
} from '../lib/envTabs'
import * as api from '../api'
import { buildHostCheck, updatableCount } from '../store/updateCheck'
import { fetchIntegrationUpdates, type IntegrationHost } from '../api/integrationUpdates'
import { herdrHostBlocked } from '../store/herdrUpdate'
import { useT } from '../lib/i18n'

import { OnboardingChecklist } from './OnboardingChecklist'
import { useOnboarding } from '../lib/useOnboarding'
import { HostsPanel } from './HostsPanel'
import { UpdateCheckPanel } from './UpdateCheckPanel'
import { IdentitiesPanel } from './IdentitiesPanel'
import { DeletedBotsPanel } from './DeletedBotsPanel'
import { JudgePanel } from './JudgePanel'
import { ClaudeApiPanel } from './ClaudeApiPanel'
import { KindDisplayToggle } from './KindTag'
import { KeepAwakeToggle } from './KeepAwakeToggle'
import { LanguageToggle } from './LanguageToggle'
import { ModelModePanel } from './ModelModePanel'
import './envShell.css'

export function EnvShell() {
  const t = useT()
  const hosts = useStore((s) => s.hosts)
  const localTools = useStore((s) => s.localTools)
  const localHerdr = useStore((s) => s.localHerdr)
  const upstream = useStore((s) => s.upstreamUpdates)
  const bots = useStore((s) => s.bots)

  const onboarding = useOnboarding()

  // 狀態字收集
  const [integrationSnap, setIntegrationSnap] = useState<IntegrationHost[] | null>(null)
  const [deletedCount, setDeletedCount] = useState<number | null>(null)

  // 1. 主機未連線數（與側欄同一式）
  const hostsDown = hosts.filter((h) => !h.connected).length

  // 2. 更新檢查：載入 integration 快照以精確計算可更新數
  useEffect(() => {
    let live = true
    fetchIntegrationUpdates()
      .then((hosts) => {
        if (live) setIntegrationSnap(hosts)
      })
      .catch(() => {
        // 失敗時保持 null，不崩潰
      })
    return () => {
      live = false
    }
  }, [])

  let updatesCount = 0
  if (upstream) {
    const shared = new Set(hosts.filter((h) => h.shared_session).map((h) => h.name))
    const localCheck = buildHostCheck({
      host: 'local',
      tools: localTools,
      herdr: localHerdr,
      upstream,
      integration: integrationSnap?.find((h) => h.host === 'local') ?? null,
      herdrBlocked: herdrHostBlocked('local', shared),
    })
    updatesCount += updatableCount(localCheck.rows)
    for (const h of hosts) {
      const hc = buildHostCheck({
        host: h.name,
        tools: h.tools,
        herdr: h.herdr,
        upstream,
        integration: integrationSnap?.find((item) => item.host === h.name) ?? null,
        herdrBlocked: herdrHostBlocked(h.name, shared),
      })
      updatesCount += updatableCount(hc.rows)
    }
  }

  // 3. 最近刪除數
  useEffect(() => {
    let live = true
    api
      .fetchDeletedBots()
      .then((r) => {
        if (live) setDeletedCount(r.length)
      })
      .catch(() => {
        if (live) setDeletedCount(null)
      })
    return () => {
      live = false
    }
  }, [bots])

  // 整理 badges
  const badges: EnvTabBadges = {
    setup: onboarding.needed && onboarding.pending > 0 ? { text: t('{n} 待處理', { n: onboarding.pending }), tone: 'warn' } : null,
    hosts: hostsDown > 0 ? { text: t('{n} 未連線', { n: hostsDown }), tone: 'bad' } : null,
    updates: updatesCount > 0 ? { text: t('{n} 可更新', { n: updatesCount }), tone: 'warn' } : null,
    deleted: deletedCount !== null && deletedCount > 0 ? { text: t('{n} 個', { n: deletedCount }), tone: 'dim' } : null,
  }

  // 決定當前分頁（初次載入由 resolveInitialEnvTab 計算）
  const [activeTab, setActiveTab] = useState<EnvTabId>(() => resolveInitialEnvTab(badges, getRememberedTab()))

  const selectTab = (id: EnvTabId) => {
    setActiveTab(id)
    rememberTab(id)
  }

  // 鍵盤導覽：ArrowUp / ArrowDown / ArrowLeft / ArrowRight / Home / End
  const handleKeyDown = (e: KeyboardEvent<HTMLButtonElement>, currentIndex: number) => {
    let nextIndex = currentIndex
    if (e.key === 'ArrowDown' || e.key === 'ArrowRight') {
      e.preventDefault()
      nextIndex = (currentIndex + 1) % ENV_TABS.length
    } else if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') {
      e.preventDefault()
      nextIndex = (currentIndex - 1 + ENV_TABS.length) % ENV_TABS.length
    } else if (e.key === 'Home') {
      e.preventDefault()
      nextIndex = 0
    } else if (e.key === 'End') {
      e.preventDefault()
      nextIndex = ENV_TABS.length - 1
    }

    if (nextIndex !== currentIndex) {
      const target = ENV_TABS[nextIndex]
      if (target) {
        selectTab(target.id)
        const btn = document.getElementById(`env-tab-${target.id}`)
        btn?.focus()
      }
    }
  }

  return (
    <div className="env-shell env-panel">
      <div className="env-nav" role="tablist" aria-label={t('環境設定分類')}>
        {ENV_TABS.map((tab, idx) => {
          const isSelected = activeTab === tab.id
          const badge = badges[tab.id]
          return (
            <button
              type="button"
              key={tab.id}
              role="tab"
              id={`env-tab-${tab.id}`}
              className="env-nav-btn"
              aria-controls={`env-panel-${tab.id}`}
              aria-selected={isSelected}
              onClick={() => selectTab(tab.id)}
              onKeyDown={(e) => handleKeyDown(e, idx)}
            >
              <span className="env-nav-label">{t.tr(tab.label)}</span>
              {badge ? (
                <span className={`env-nav-meta${badge.tone ? ` ${badge.tone}` : ''}`}>{badge.text}</span>
              ) : null}
            </button>
          )
        })}
      </div>

      <div className="env-main">
        <section
          role="tabpanel"
          id="env-panel-setup"
          aria-labelledby="env-tab-setup"
          className="env-tabpanel"
          hidden={activeTab !== 'setup'}
        >
          <h2 className="page-title">{t('環境準備')}</h2>
          <OnboardingChecklist />
        </section>

        <section
          role="tabpanel"
          id="env-panel-hosts"
          aria-labelledby="env-tab-hosts"
          className="env-tabpanel"
          hidden={activeTab !== 'hosts'}
        >
          <h2 className="page-title">{t('主機')}</h2>
          <HostsPanel />
        </section>

        <section
          role="tabpanel"
          id="env-panel-updates"
          aria-labelledby="env-tab-updates"
          className="env-tabpanel"
          hidden={activeTab !== 'updates'}
        >
          <h2 className="page-title">{t('更新檢查')}</h2>
          <UpdateCheckPanel />
        </section>

        <section
          role="tabpanel"
          id="env-panel-identities"
          aria-labelledby="env-tab-identities"
          className="env-tabpanel"
          hidden={activeTab !== 'identities'}
        >
          <h2 className="page-title">{t('身份')}</h2>
          <IdentitiesPanel />
        </section>

        <section
          role="tabpanel"
          id="env-panel-deleted"
          aria-labelledby="env-tab-deleted"
          className="env-tabpanel"
          hidden={activeTab !== 'deleted'}
        >
          <h2 className="page-title">{t('最近刪除')}</h2>
          <DeletedBotsPanel />
        </section>

        <section
          role="tabpanel"
          id="env-panel-judge"
          aria-labelledby="env-tab-judge"
          className="env-tabpanel"
          hidden={activeTab !== 'judge'}
        >
          <h2 className="page-title">{t('Jev 建議')}</h2>
          <JudgePanel />
        </section>

        <section
          role="tabpanel"
          id="env-panel-claude-api"
          aria-labelledby="env-tab-claude-api"
          className="env-tabpanel"
          hidden={activeTab !== 'claude-api'}
        >
          <h2 className="page-title">Claude API</h2>
          <ClaudeApiPanel />
        </section>

        <section
          role="tabpanel"
          id="env-panel-display"
          aria-labelledby="env-tab-display"
          className="env-tabpanel"
          hidden={activeTab !== 'display'}
        >
          <h2 className="page-title">{t('顯示')}</h2>
          <LanguageToggle />
          <KindDisplayToggle />
          <KeepAwakeToggle />
          <ModelModePanel />
        </section>
      </div>
    </div>
  )
}
