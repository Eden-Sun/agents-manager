export type StopHookStatus = 'not_installed' | 'untrusted' | 'hash_mismatch' | 'trusted' | 'unavailable'

export interface StopHookEntry {
  host: string
  identity: string | null
  codex_home: string
  status: StopHookStatus
}

export interface StopHookReport {
  enabled: boolean
  entries: StopHookEntry[]
}

export async function fetchStopHook(): Promise<StopHookReport> {
  const res = await fetch('/api/codex/stop-hook')
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return res.json()
}

export async function trustStopHook(host: string, identity: string | null): Promise<StopHookEntry> {
  const res = await fetch('/api/codex/stop-hook/trust', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ host, identity }),
  })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return res.json()
}
