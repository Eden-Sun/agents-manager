import { msg } from './i18n'

export type EnvTabId = 'setup' | 'hosts' | 'updates' | 'identities' | 'deleted' | 'judge' | 'claude-api' | 'display'

export interface EnvTabMeta {
  id: EnvTabId
  label: string
  panelTitle: string
}

export const ENV_TABS: readonly EnvTabMeta[] = [
  { id: 'setup', label: msg('環境準備'), panelTitle: msg('環境準備') },
  { id: 'hosts', label: msg('主機'), panelTitle: msg('主機') },
  { id: 'updates', label: msg('更新檢查'), panelTitle: msg('更新檢查') },
  { id: 'identities', label: msg('身分'), panelTitle: msg('身分') },
  { id: 'deleted', label: msg('最近刪除'), panelTitle: msg('最近刪除') },
  { id: 'judge', label: msg('Jev 建議'), panelTitle: msg('Jev 建議') },
  { id: 'claude-api', label: msg('Claude API'), panelTitle: msg('Claude API') },
  { id: 'display', label: msg('顯示'), panelTitle: msg('顯示') },
] as const

export interface EnvTabBadge {
  text: string
  tone?: 'bad' | 'warn' | 'dim'
}

export type EnvTabBadges = Partial<Record<EnvTabId, EnvTabBadge | null>>

export const ENV_TAB_SESSION_KEY = 'am-env-active-tab'

/**
 * 記憶體回退：若 sessionStorage 不可用（如私密模式被擋或非瀏覽器環境）時使用
 */
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
    // 忽略 sessionStorage 錯誤
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
    // 忽略 sessionStorage 錯誤
  }
}

/** 供測試或重置時清除記憶體與 sessionStorage 中的狀態 */
export function clearRememberedTab(): void {
  inMemoryTab = null
  try {
    if (typeof sessionStorage !== 'undefined') {
      sessionStorage.removeItem(ENV_TAB_SESSION_KEY)
    }
  } catch {
    // 忽略
  }
}

/**
 * 判斷預設分頁：
 * 1. 同一瀏覽器分頁內記住上次停的分頁（rememberedTab），若有則優先使用（代表不是該 session 第一次打開）
 * 2. 否則（該 session 第一次打開），依導覽順序挑選第一個有紅（bad）或黃（warn）狀態字的分頁
 * 3. 都沒有時停「主機」（'hosts'）
 */
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
