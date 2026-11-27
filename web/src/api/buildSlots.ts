import { rawTransport } from './index'

export interface BuildSlot {
  holder: string
  status: 'held' | 'waiting' | string
  bot_id: string | null
  purpose: string | null
  host: string
  since: string
}

export interface BuildRemoteHost {
  target: string
  local: boolean
  active?: number | null
  waiting?: number | null
}

export interface RemoteRun {
  call_id: string
  bot_id: string | null
  purpose: string
  since: string
  target: string | null
}

export interface BuildSlots {
  max_concurrent: number
  active: number
  slots: BuildSlot[]
  remote: { enabled: boolean; hosts: BuildRemoteHost[] }
  remote_runs?: RemoteRun[]
  total_active?: number
  total_waiting?: number
}

export async function fetchBuildSlots(): Promise<BuildSlots> {
  return (await rawTransport.request('GET', '/build-slots')) as BuildSlots
}
