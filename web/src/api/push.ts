import { rawTransport } from './index'

/** 網頁推播（API.md「網頁推播」）；全部 User-only。 */
export async function getVapidKey(): Promise<string> {
  const r = (await rawTransport.request('GET', '/push/vapid-key')) as { public_key?: string }
  if (!r?.public_key) throw new Error('no vapid key')
  return r.public_key
}

export async function listPushEndpoints(): Promise<string[]> {
  const r = (await rawTransport.request('GET', '/push/subscriptions')) as { subscriptions?: { endpoint?: string }[] }
  return (r?.subscriptions ?? []).map((s) => s.endpoint ?? '').filter(Boolean)
}

export async function savePushSubscription(sub: { endpoint: string; keys: { p256dh: string; auth: string }; origin?: string }): Promise<void> {
  await rawTransport.request('POST', '/push/subscriptions', sub)
}

export async function deletePushSubscription(endpoint: string): Promise<void> {
  await rawTransport.request('DELETE', `/push/subscriptions?endpoint=${encodeURIComponent(endpoint)}`)
}

export async function sendTestPush(): Promise<{ sent: number; removed: number; failed: number; reasons?: string[] }> {
  return (await rawTransport.request('POST', '/push/test')) as { sent: number; removed: number; failed: number; reasons?: string[] }
}

export type PushSettingsData = PushSettingsValue

export async function getPushSettings(): Promise<PushSettingsValue> {
  return (await rawTransport.request('GET', '/push/settings')) as PushSettingsValue
}

export interface PushSettingsValue {
  main_bots: boolean
  hosts: boolean
  other_bots: boolean
  updates: boolean
  reset_credits: boolean
  auth: boolean
  quota: boolean
  schedules: boolean
  ops: boolean
}

export async function setPushSettings(s: Partial<PushSettingsValue>): Promise<PushSettingsValue> {
  return (await rawTransport.request('PUT', '/push/settings', s)) as PushSettingsValue
}

/** 回報「這個視窗正看著哪顆 bot」：daemon 據此不對正在看的 bot 推播。 */
export async function reportPresence(p: { client_id: string; bot_id: string | null; visible: boolean; idle_secs: number }): Promise<void> {
  await rawTransport.request('POST', '/push/presence', p)
}
