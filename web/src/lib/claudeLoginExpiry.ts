import type { Host } from '../api/types'

export const THREE_DAYS_MS = 3 * 86_400_000
export const ONE_DAY_MS = 86_400_000

export type ClaudeExpiryStage = 'warn' | 'expired'

export interface ClaudeExpiryAlert {
  host: string
  account: string
  expiresAtMs: number
  stage: ClaudeExpiryStage
}

export function claudeExpiryState(expiresAtMs: number | null | undefined, nowMs: number): { stage: ClaudeExpiryStage; expiresAtMs: number } | null {
  if (typeof expiresAtMs !== 'number' || !Number.isFinite(expiresAtMs) || expiresAtMs <= 0) return null
  const rem = expiresAtMs - nowMs
  if (rem <= 0) return { stage: 'expired', expiresAtMs }
  if (rem < THREE_DAYS_MS) return { stage: 'warn', expiresAtMs }
  return null
}

export function formatExpiryTime(expiresAtMs: number, offsetHours = 8): string {
  const shifted = new Date(expiresAtMs + offsetHours * 3600_000)
  const mm = String(shifted.getUTCMonth() + 1).padStart(2, '0')
  const dd = String(shifted.getUTCDate()).padStart(2, '0')
  const hh = String(shifted.getUTCHours()).padStart(2, '0')
  const mi = String(shifted.getUTCMinutes()).padStart(2, '0')
  const sign = offsetHours >= 0 ? `+${offsetHours}` : String(offsetHours)
  return `${mm}/${dd} ${hh}:${mi} (${sign})`
}

export function collectClaudeExpiryAlerts(input: {
  localExpiry?: Record<string, number> | null
  hosts: Pick<Host, 'name' | 'claude_login_expires_at'>[]
  nowMs: number
}): ClaudeExpiryAlert[] {
  const out: ClaudeExpiryAlert[] = []
  const pushMap = (host: string, map: Record<string, number> | null | undefined) => {
    if (!map) return
    for (const account of Object.keys(map).sort()) {
      const st = claudeExpiryState(map[account], input.nowMs)
      if (st) out.push({ host, account, expiresAtMs: st.expiresAtMs, stage: st.stage })
    }
  }
  pushMap('local', input.localExpiry)
  for (const h of input.hosts) {
    if (h.name === 'local') continue
    pushMap(h.name, h.claude_login_expires_at)
  }
  return out
}

export function expiryNoticeText(alert: ClaudeExpiryAlert, hostLabel: string, offsetHours = 8): string {
  if (alert.stage === 'expired') {
    return `${alert.account}（${hostLabel}）的 Claude 登入已過期`
  }
  return `${alert.account}（${hostLabel}）的 Claude 登入將於 ${formatExpiryTime(alert.expiresAtMs, offsetHours)} 到期`
}
