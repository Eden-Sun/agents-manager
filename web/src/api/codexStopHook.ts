import { rawTransport } from './index'

export type StopHookStatus = 'not_installed' | 'untrusted' | 'hash_mismatch' | 'trusted' | 'unavailable'

export interface StopHookEntry {
  host: string
  identity: string | null
  codex_home: string
  status: StopHookStatus
  kill_guard?: StopHookStatus
}

export interface StopHookReport {
  enabled: boolean
  entries: StopHookEntry[]
}

export function fetchStopHook(): Promise<StopHookReport> {
  return rawTransport.request('GET', '/codex/stop-hook') as Promise<StopHookReport>
}

export function trustStopHook(host: string, identity: string | null): Promise<StopHookEntry> {
  return rawTransport.request('POST', '/codex/stop-hook/trust', { host, identity }) as Promise<StopHookEntry>
}
