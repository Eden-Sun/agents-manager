import type { HerdrVersion } from '../api/types'
import { t } from './i18n'

export type HerdrVersionLevel = 'ok' | 'warn' | 'unknown'

export interface HerdrVersionView {
  text: string
  level: HerdrVersionLevel
  hint: string
}

export function herdrVersionView(h: HerdrVersion): HerdrVersionView {
  const { server_version: sv, protocol, cli_version: cli } = h
  if (!sv && !cli) return { text: t('herdr 版本：未知'), level: 'unknown', hint: t('主機沒連上或讀不到 herdr 版本') }
  const base = sv
    ? `herdr ${sv}${protocol != null ? ` · protocol ${protocol}` : ''}`
    : t('herdr CLI {cli}（server 版本未知）', { cli: cli ?? '?' })
  if (h.mismatch) {
    return {
      text: `${base} ⚠ CLI ${cli}`,
      level: 'warn',
      hint: t('CLI 是 {cli}，但跑著的 herdr server 還是 {sv}：bot 的 herdr 指令會回 protocol_mismatch，要重啟 herdr server 才會換到新版。', { cli: cli ?? '?', sv: sv ?? '?' }),
    }
  }
  if (h.protocol_supported === false) {
    return { text: `${base} ⚠ ${t('未驗證')}`, level: 'warn', hint: t('protocol {protocol} 不在 daemon 實測過的清單，RPC 形狀沒驗過。', { protocol: protocol ?? '?' }) }
  }
  return { text: base, level: sv ? 'ok' : 'unknown', hint: '' }
}
