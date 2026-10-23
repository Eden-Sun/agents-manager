import type { UpstreamItem } from '../store/upstreamUpdate'
import { t } from './i18n'

/**
 * header 的「grok 有新版」（issue #761，daemon `upstream_update.rs`）。grok 官方的升級指令只有 `grok update`
 * （自己的 installer；`--check` 只查不裝），而且要在**那台主機**上跑、跑著的 bot 要重啟才換版，所以這顆只提示、不做一鍵安裝。
 */
export const GROK_UPDATE_COMMAND = 'grok update'

export interface GrokUpdatePlan {
  target: string
  /** 落後（或讀不到版本）的主機；`from` 是那台磁碟上的版本。 */
  hosts: { host: string; from: string | null }[]
  command: string
}

export function grokUpdatePlan(item: UpstreamItem | null | undefined): GrokUpdatePlan | null {
  if (!item || item.kind !== 'grok' || !item.hasUpdate || !item.target) return null
  const hosts = item.hosts.filter((h) => h.behind).map((h) => ({ host: h.host, from: h.installedVersion }))
  if (hosts.length === 0) return null
  return { target: item.target, hosts, command: GROK_UPDATE_COMMAND }
}

/** chip 的 tooltip／aria-label。 */
export function grokChipLabel(plan: GrokUpdatePlan): string {
  const hosts = plan.hosts.map((h) => `${h.host}${t('：')}${h.from ?? t('讀不到版本')}`).join(t('；'))
  return t('grok 有新版 {target}（{hosts}）· 在那台主機執行 `{command}`，已在跑的 bot 重啟後才會換版', { target: plan.target, hosts, command: plan.command })
}

/** 手機合成選單裡 grok 那一項的文案（只提示，點開看指令）。 */
export function grokMenuItem(plan: GrokUpdatePlan): string {
  const hostList = plan.hosts.map((h) => h.host).join(t('、'))
  return t('grok {target}：{hosts} 還沒升（只提示，指令 {command}）', { target: plan.target, hosts: hostList, command: plan.command })
}
