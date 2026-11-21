import type { UpstreamItem } from '../store/upstreamUpdate'
import { t } from './i18n'

export interface AgyUpdatePlan {
  target: string
  hosts: { host: string; from: string | null }[]
}

export function agyUpdatePlan(item: UpstreamItem | null | undefined): AgyUpdatePlan | null {
  if (!item || item.autoUpdate || item.kind !== 'agy' || !item.hasUpdate || !item.target) return null
  const hosts = item.hosts.filter((h) => h.behind).map((h) => ({ host: h.host, from: h.installedVersion }))
  if (hosts.length === 0) return null
  return { target: item.target, hosts }
}

export function agyUpdateTitle(target: string): string {
  return t('更新 Antigravity（agy）CLI 到 {target}？', { target })
}

export function agyChipLabel(plan: AgyUpdatePlan): string {
  const hosts = plan.hosts.map((h) => `${h.host}${t('：')}${h.from ?? t('讀不到版本')}`).join(t('；'))
  return t('agy 有新版 {target}（{hosts}）· 點一下更新：從官方下載、驗 sha512 後換掉 ~/.local/bin/agy；跑著的 agy bot 重啟後才換版', { target: plan.target, hosts })
}

export function agyMenuItem(plan: AgyUpdatePlan): string {
  const hostList = plan.hosts.map((h) => h.host).join(t('、'))
  return t('agy {target}：{hosts} 可更新（點開確認）', { target: plan.target, hosts: hostList })
}
