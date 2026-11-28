import { rawTransport } from './index'

export interface AutoUpdateSettings {
  claude: boolean
  codex: boolean
  agy: boolean
  grok: boolean
}

export function fetchAutoUpdateSettings(): Promise<AutoUpdateSettings> {
  return rawTransport.request('GET', '/settings/auto-update') as Promise<AutoUpdateSettings>
}

export function updateAutoUpdateSettings(partial: Partial<AutoUpdateSettings>): Promise<AutoUpdateSettings> {
  return rawTransport.request('PUT', '/settings/auto-update', partial) as Promise<AutoUpdateSettings>
}
