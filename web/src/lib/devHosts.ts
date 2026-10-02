/**
 * vite dev server（5173，`host: true`＝對外）允許的 Host 名稱，跟 `server.allowedHosts` 同一份。
 * IP 與 `localhost` 永遠放行（vite 自己的規則）；其餘只放行自己 tailnet 的名字，不整個關掉檢查（那是擋 DNS rebinding 的）。
 */
export const DEV_ALLOWED_HOSTS = ['agm', '.ts.net']

/** vite 的 Host 規則（IP、localhost、`allowedHosts`；`.` 開頭的條目＝整個後綴，連 apex 本身）。給 vite 管不到的路徑用（proxy 的 WebSocket upgrade）。 */
export function devHostAllowed(hostHeader: string | undefined, allowed: readonly string[]): boolean {
  if (!hostHeader) return false
  const a = hostHeader.trim().toLowerCase()
  const host = a.startsWith('[') ? a.slice(0, a.indexOf(']') + 1) : a.replace(/:\d+$/, '')
  if (!host) return false
  if (/^\[[0-9a-f:.]+\]$/.test(host) || /^\d{1,3}(\.\d{1,3}){3}$/.test(host) || host === 'localhost') return true
  return allowed.some(entry => {
    const e = entry.toLowerCase()
    return e.startsWith('.') ? host === e.slice(1) || host.endsWith(e) : host === e
  })
}
