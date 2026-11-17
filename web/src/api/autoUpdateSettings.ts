export interface AutoUpdateSettings {
  claude: boolean
  codex: boolean
  agy: boolean
  grok: boolean
}

export async function fetchAutoUpdateSettings(): Promise<AutoUpdateSettings> {
  const res = await fetch('/api/settings/auto-update')
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return res.json()
}

export async function updateAutoUpdateSettings(partial: Partial<AutoUpdateSettings>): Promise<AutoUpdateSettings> {
  const res = await fetch('/api/settings/auto-update', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(partial),
  })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return res.json()
}
