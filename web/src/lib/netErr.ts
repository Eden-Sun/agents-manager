import { t } from './i18n'

const NETWORK_MSG = /failed to fetch|networkerror|load failed|network request failed|fetch failed/i

export function networkErrText(e: unknown): string | null {
  const name = (e as { name?: unknown } | null)?.name
  if (name === 'AbortError' || name === 'TimeoutError') return t('連線逾時：daemon 沒有回應，稍後再試（你打的字還在）')
  if (e instanceof TypeError && NETWORK_MSG.test(e.message)) {
    return t('連不上 daemon（可能正在重啟，或網路斷了），稍後再試（你打的字還在）')
  }
  return null
}

export function gatewayErrText(status: number, hasDaemonMessage: boolean): string | null {
  if (hasDaemonMessage) return null
  if (status === 502 || status === 503 || status === 504) return t('daemon 暫時不可用（HTTP {status}），稍後再試', { status })
  return null
}
