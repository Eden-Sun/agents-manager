import type { UpstreamItem } from '../store/upstreamUpdate'
import { t } from './i18n'

export const GROK_UPDATE_COMMAND = 'grok update'

export interface GrokUpdatePlan {
  target: string
  hosts: { host: string; from: string | null }[]
  command: string
}

export function grokUpdatePlan(item: UpstreamItem | null | undefined): GrokUpdatePlan | null {
  if (!item || item.autoUpdate || item.kind !== 'grok' || !item.hasUpdate || !item.target) return null
  const hosts = item.hosts.filter((h) => h.behind).map((h) => ({ host: h.host, from: h.installedVersion }))
  if (hosts.length === 0) return null
  return { target: item.target, hosts, command: GROK_UPDATE_COMMAND }
}

export function grokChipLabel(plan: GrokUpdatePlan): string {
  const hosts = plan.hosts.map((h) => `${h.host}${t('：')}${h.from ?? t('讀不到版本')}`).join(t('；'))
  return t('grok 有新版 {target}（{hosts}）· 在那台主機執行 `{command}`，已在跑的 bot 重啟後才會換版', { target: plan.target, hosts, command: plan.command })
}

export function grokMenuItem(plan: GrokUpdatePlan): string {
  const hostList = plan.hosts.map((h) => h.host).join(t('、'))
  return t('grok {target}：{hosts} 還沒升（只提示，指令 {command}）', { target: plan.target, hosts: hostList, command: plan.command })
}
