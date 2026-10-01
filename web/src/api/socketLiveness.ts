/**
 * WS 存活偵測（issue #760）。瀏覽器看不到 WebSocket 的 ping 控制幀，所以 daemon 每 `PING_EVERY_MS`（20 秒，
 * `state::WS_PING_EVERY_MS`）送一個 `{"type":"ping"}` text 幀；任何幀（含 ping）都算「線還活著」。
 * 半開連線（筆電睡眠、NAT／tailscale 映射逾時、手機換網路）下瀏覽器不一定收到 close，`readyState` 一直是 OPEN，
 * 之後的事件全部收不到；靠它判斷「死了」。
 */

/** 前景裡連續這麼久沒收到任何幀（含 ping）＝半開，主動重連。心跳 20 秒，容許掉兩個。 */
export const SILENCE_MS = 60_000

/** 切回前景（visible／focus／online）時，靜默超過這麼久就不信 `readyState`，直接重連：心跳 20 秒加餘裕。 */
export const RESUME_STALE_MS = 35_000

/** 看門狗多久檢查一次。 */
export const CHECK_MS = 10_000

export interface LivenessDeps {
  now: () => number
  /** 每 `ms` 呼叫 `fn`，回傳停止函式；測試換成手動觸發。 */
  every: (fn: () => void, ms: number) => () => void
}

export const realLivenessDeps: LivenessDeps = {
  now: () => Date.now(),
  every: (fn, ms) => {
    const id = setInterval(fn, ms)
    return () => clearInterval(id)
  },
}

export function isForeground(): boolean {
  return typeof document === 'undefined' || document.visibilityState !== 'hidden'
}
