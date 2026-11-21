import { msg } from './i18n'

export type EnvTabId = 'setup' | 'hosts' | 'updates' | 'identities' | 'judge' | 'claude-api' | 'openai-api' | 'display'

export interface EnvTabMeta {
  id: EnvTabId
  label: string
  panelTitle: string
}

export const ENV_TABS: readonly EnvTabMeta[] = [
  { id: 'setup', label: msg('環境準備'), panelTitle: msg('環境準備') },
  { id: 'hosts', label: msg('主機'), panelTitle: msg('主機') },
  { id: 'updates', label: msg('更新檢查'), panelTitle: msg('更新檢查') },
  { id: 'identities', label: msg('身份'), panelTitle: msg('身份') },
  { id: 'judge', label: msg('Jev 建議'), panelTitle: msg('Jev 建議') },
  { id: 'claude-api', label: msg('Claude API'), panelTitle: msg('Claude API') },
  { id: 'openai-api', label: msg('OpenAI API'), panelTitle: msg('OpenAI API') },
  { id: 'display', label: msg('顯示'), panelTitle: msg('顯示') },
] as const

export interface EnvTabBadge {
  text: string
  tone?: 'bad' | 'warn' | 'dim'
}

export type EnvTabBadges = Partial<Record<EnvTabId, EnvTabBadge | null>>

export const ENV_TAB_SESSION_KEY = 'am-env-active-tab'

let inMemoryTab: EnvTabId | null = null

export function getRememberedTab(): EnvTabId | null {
  try {
    if (typeof sessionStorage !== 'undefined') {
      const val = sessionStorage.getItem(ENV_TAB_SESSION_KEY)
      if (val && ENV_TABS.some((t) => t.id === val)) {
        return val as EnvTabId
      }
    }
  } catch {
  }
  return inMemoryTab
}

export function rememberTab(id: EnvTabId): void {
  inMemoryTab = id
  try {
    if (typeof sessionStorage !== 'undefined') {
      sessionStorage.setItem(ENV_TAB_SESSION_KEY, id)
    }
  } catch {
  }
}

export function clearRememberedTab(): void {
  inMemoryTab = null
  try {
    if (typeof sessionStorage !== 'undefined') {
      sessionStorage.removeItem(ENV_TAB_SESSION_KEY)
    }
  } catch {
  }
}

export function resolveInitialEnvTab(badges: EnvTabBadges, remembered: EnvTabId | null = getRememberedTab()): EnvTabId {
  if (remembered) {
    return remembered
  }

  for (const tab of ENV_TABS) {
    const badge = badges[tab.id]
    if (badge && (badge.tone === 'bad' || badge.tone === 'warn')) {
      return tab.id
    }
  }

  return 'hosts'
}
