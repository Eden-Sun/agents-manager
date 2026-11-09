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

export async function savePushSubscription(sub: { endpoint: string; keys: { p256dh: string; auth: string } }): Promise<void> {
  await rawTransport.request('POST', '/push/subscriptions', sub)
}

export async function deletePushSubscription(endpoint: string): Promise<void> {
  await rawTransport.request('DELETE', `/push/subscriptions?endpoint=${encodeURIComponent(endpoint)}`)
}

export async function sendTestPush(): Promise<{ sent: number; removed: number; failed: number }> {
  return (await rawTransport.request('POST', '/push/test')) as { sent: number; removed: number; failed: number }
}
