import type { HerdrVersion } from '../api/types'

export type HerdrVersionLevel = 'ok' | 'warn' | 'unknown'

export interface HerdrVersionView {
  text: string
  level: HerdrVersionLevel
  /** tooltip／說明；沒有就不顯示 */
  hint: string
}

/**
 * hosts 面板的 herdr 版本行（docs/UI-DECISIONS.md）：`herdr 0.9.1 · protocol 22`。
 * CLI 與 server 版本不同、或 protocol 不在 daemon 實測過的清單 → warn；讀不到 → 「未知」，不猜。
 */
export function herdrVersionView(h: HerdrVersion): HerdrVersionView {
  const { server_version: sv, protocol, cli_version: cli } = h
  if (!sv && !cli) return { text: 'herdr 版本：未知', level: 'unknown', hint: '主機沒連上或讀不到 herdr 版本' }
  const base = sv
    ? `herdr ${sv}${protocol != null ? ` · protocol ${protocol}` : ''}`
    : `herdr CLI ${cli}（server 版本未知）`
  if (h.mismatch) {
    return {
      text: `${base} ⚠ CLI ${cli}`,
      level: 'warn',
      hint: `CLI 是 ${cli}，但跑著的 herdr server 還是 ${sv}：bot 的 herdr 指令會回 protocol_mismatch，要重啟 herdr server 才會換到新版。`,
    }
  }
  if (h.protocol_supported === false) {
    return { text: `${base} ⚠ 未驗證`, level: 'warn', hint: `protocol ${protocol} 不在 daemon 實測過的清單，RPC 形狀沒驗過。` }
  }
  return { text: base, level: sv ? 'ok' : 'unknown', hint: '' }
}
