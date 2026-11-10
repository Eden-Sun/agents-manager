/** 網頁推播的瀏覽器端：支援度判斷、訂閱／取消、點通知開排程。service worker 在 `public/sw.js`（只處理 push 與 click）。 */
import { deletePushSubscription, getVapidKey, reportPresence, savePushSubscription } from '../api/push'

export type PushSupport = { ok: true } | { ok: false; reason: 'insecure' | 'unsupported' | 'ios-home-screen' }

function isIos(nav: Navigator): boolean {
  return /iPad|iPhone|iPod/.test(nav.userAgent) || (nav.platform === 'MacIntel' && (nav.maxTouchPoints ?? 0) > 1)
}

/** 可否使用推播：要 secure context、Service Worker、PushManager；iPhone 還要以主畫面（standalone）開啟。 */
export function pushSupport(env: { win?: Window; nav?: Navigator } = {}): PushSupport {
  const win = env.win ?? window
  const nav = env.nav ?? navigator
  if (!win.isSecureContext) return { ok: false, reason: 'insecure' }
  const standalone = (nav as Navigator & { standalone?: boolean }).standalone === true || win.matchMedia?.('(display-mode: standalone)').matches === true
  if (isIos(nav) && !standalone) return { ok: false, reason: 'ios-home-screen' }
  if (!('serviceWorker' in nav) || !(win as unknown as Record<string, unknown>).PushManager || !(win as unknown as Record<string, unknown>).Notification) return { ok: false, reason: 'unsupported' }
  return { ok: true }
}

export function urlBase64ToUint8Array(b64: string): Uint8Array {
  const pad = '='.repeat((4 - (b64.length % 4)) % 4)
  const raw = atob((b64 + pad).replace(/-/g, '+').replace(/_/g, '/'))
  return Uint8Array.from(raw, (c) => c.charCodeAt(0))
}

async function registration(): Promise<ServiceWorkerRegistration> {
  await navigator.serviceWorker.register('/sw.js')
  return navigator.serviceWorker.ready
}

/** 這個瀏覽器目前的訂閱（沒有註冊過 service worker 就是 null，不會主動註冊）。 */
export async function currentSubscription(): Promise<PushSubscription | null> {
  const reg = await navigator.serviceWorker.getRegistration('/')
  return reg ? reg.pushManager.getSubscription() : null
}

/** 要求權限 → 訂閱 → 送到 daemon。權限被拒回 'denied'。 */
export async function enablePush(): Promise<'ok' | 'denied'> {
  const perm = await Notification.requestPermission()
  if (perm !== 'granted') return 'denied'
  const reg = await registration()
  const key = urlBase64ToUint8Array(await getVapidKey())
  const sub = (await reg.pushManager.getSubscription()) ?? (await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key as BufferSource }))
  const json = sub.toJSON() as { endpoint?: string; keys?: { p256dh?: string; auth?: string } }
  if (!json.endpoint || !json.keys?.p256dh || !json.keys.auth) throw new Error('incomplete subscription')
  await savePushSubscription({ endpoint: json.endpoint, keys: { p256dh: json.keys.p256dh, auth: json.keys.auth }, origin: window.location.origin })
  return 'ok'
}

export async function disablePush(): Promise<void> {
  const sub = await currentSubscription()
  if (!sub) return
  const endpoint = sub.endpoint
  await sub.unsubscribe()
  await deletePushSubscription(endpoint)
}

/** 通知的 url（`/?schedule=…&project=…&occurrence=…`）→ 開排程的事件參數。 */
export function parseScheduleUrl(url: string): { projectId: string; scheduleId: string; occurrenceId?: string } | null {
  try {
    const q = new URL(url, 'http://x').searchParams
    const scheduleId = q.get('schedule')
    const projectId = q.get('project')
    return scheduleId && projectId ? { projectId, scheduleId, occurrenceId: q.get('occurrence') ?? undefined } : null
  } catch {
    return null
  }
}

/** 通知的 url（`/bots/<id>`）→ bot id。 */
export function parseBotUrl(url: string): string | null {
  try {
    const m = /^\/bots\/([^/]+)\/?$/.exec(new URL(url, 'http://x').pathname)
    return m ? decodeURIComponent(m[1]) : null
  } catch {
    return null
  }
}

/** 通知的 url（`/?reset_offer=<額度 key>`）→ 額度 key。 */
export function parseResetOfferUrl(url: string): string | null {
  try {
    return new URL(url, 'http://x').searchParams.get('reset_offer')
  } catch {
    return null
  }
}

/** service worker 在已開啟的視窗裡點通知時 postMessage：轉成 `agm:open-schedule`（側欄聽它）。 */
export function installPushClickBridge(): () => void {
  if (typeof navigator === 'undefined' || !('serviceWorker' in navigator)) return () => {}
  const onMessage = (ev: MessageEvent) => {
    const d = ev.data as { type?: string; url?: string } | null
    if (d?.type !== 'agm-open-url' || !d.url) return
    const target = parseScheduleUrl(d.url)
    if (target) window.dispatchEvent(new CustomEvent('agm:open-schedule', { detail: target }))
    const offer = parseResetOfferUrl(d.url)
    if (offer) window.dispatchEvent(new CustomEvent('agm:open-reset-offer', { detail: { key: offer } }))
    const botId = parseBotUrl(d.url)
    if (botId) window.dispatchEvent(new CustomEvent('agm:open-bot', { detail: { botId } }))
  }
  navigator.serviceWorker.addEventListener('message', onMessage)
  return () => navigator.serviceWorker.removeEventListener('message', onMessage)
}

/**
 * 把「這個視窗可見、選中哪顆 bot、距上次互動幾秒」定期回報給 daemon（主力 bot 推播據此不打擾正在看的人）。
 * 沒有訂閱推播的瀏覽器也報，反正 daemon 只拿來抑制；失敗忽略（沒有 presence＝照推）。
 */
export function installPresenceReporter(getBotId: () => string | null): () => void {
  if (typeof document === 'undefined') return () => {}
  const id = (() => {
    try {
      const k = 'agm-presence-id'
      return sessionStorage.getItem(k) ?? (sessionStorage.setItem(k, crypto.randomUUID()), sessionStorage.getItem(k)!)
    } catch {
      return Math.random().toString(36).slice(2)
    }
  })()
  let lastInput = Date.now()
  const touch = () => {
    lastInput = Date.now()
  }
  const send = () => {
    void reportPresence({ client_id: id, bot_id: getBotId(), visible: document.visibilityState === 'visible', idle_secs: Math.round((Date.now() - lastInput) / 1000) }).catch(() => {})
  }
  const evs = ['pointerdown', 'keydown', 'touchstart', 'wheel'] as const
  evs.forEach((e) => window.addEventListener(e, touch, { passive: true }))
  document.addEventListener('visibilitychange', send)
  const timer = window.setInterval(send, 20_000)
  send()
  return () => {
    evs.forEach((e) => window.removeEventListener(e, touch))
    document.removeEventListener('visibilitychange', send)
    window.clearInterval(timer)
  }
}
