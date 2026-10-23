import type { UpstreamItem } from '../store/upstreamUpdate'
import { t } from './i18n'

/**
 * header 的「agy 有新版」（issue #1921）：agy 的上游版來自官方 manifest（daemon `upstream_update.rs`，#1908）。
 * 跟 claude／codex 不同，這顆**有一鍵安裝**：按下去跟設定頁「更新檢查」同一個確認框，確認後走 `POST /api/hosts/{name}/agy/install`。
 */
export interface AgyUpdatePlan {
  target: string
  /** 落後（或讀不到版本）的主機；`from` 是那台磁碟上的版本。 */
  hosts: { host: string; from: string | null }[]
}

export function agyUpdatePlan(item: UpstreamItem | null | undefined): AgyUpdatePlan | null {
  if (!item || item.kind !== 'agy' || !item.hasUpdate || !item.target) return null
  const hosts = item.hosts.filter((h) => h.behind).map((h) => ({ host: h.host, from: h.installedVersion }))
  if (hosts.length === 0) return null
  return { target: item.target, hosts }
}

/** 確認框標題（設定頁與 header 共用）。 */
export function agyUpdateTitle(target: string): string {
  return t('更新 Antigravity（agy）CLI 到 {target}？', { target })
}

/** chip 的 tooltip／aria-label。 */
export function agyChipLabel(plan: AgyUpdatePlan): string {
  const hosts = plan.hosts.map((h) => `${h.host}：${h.from ?? t('讀不到版本')}`).join('；') // i18n-ignore
  return t('agy 有新版 {target}（{hosts}）· 點一下更新：從官方下載、驗 sha512 後換掉 ~/.local/bin/agy；跑著的 agy bot 重啟後才換版', { target: plan.target, hosts })
}

/** 手機合成選單裡 agy 那一項的文案。 */
export function agyMenuItem(plan: AgyUpdatePlan): string {
  const hostList = plan.hosts.map((h) => h.host).join('、') // i18n-ignore
  return t('agy {target}：{hosts} 可更新（點開確認）', { target: plan.target, hosts: hostList })
}
