/**
 * 連線層的失敗講人話（#370）。瀏覽器丟出來的是 `Failed to fetch`／`NetworkError when attempting to fetch resource`／
 * `Load failed`（Safari）／`The operation was aborted`，使用者看不出是 daemon 在重啟還是自己網路斷了；
 * 502／503／504 沒帶 daemon 的 JSON（前面的 proxy 或 daemon 剛掛）則只剩 `POST /api/x failed (502)`。
 * 回 null＝不是連線層的問題，交給原本的文字。
 */
const NETWORK_MSG = /failed to fetch|networkerror|load failed|network request failed|fetch failed/i

export function networkErrText(e: unknown): string | null {
  const name = (e as { name?: unknown } | null)?.name
  if (name === 'AbortError' || name === 'TimeoutError') return '連線逾時：daemon 沒有回應，稍後再試（你打的字還在）'
  if (e instanceof TypeError && NETWORK_MSG.test(e.message)) {
    return '連不上 daemon（可能正在重啟，或網路斷了），稍後再試（你打的字還在）'
  }
  return null
}

/** 沒有 daemon 自己的錯誤內容的 5xx：多半是 daemon 剛掛或重啟中。 */
export function gatewayErrText(status: number, hasDaemonMessage: boolean): string | null {
  if (hasDaemonMessage) return null
  if (status === 502 || status === 503 || status === 504) return `daemon 暫時不可用（HTTP ${status}），稍後再試`
  return null
}
