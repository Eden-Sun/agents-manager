export const DEV_ALLOWED_HOSTS = ['agm', '.ts.net']

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
