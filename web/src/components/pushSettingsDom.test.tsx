import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { PushSettings } from './PushSettings'
import { parseScheduleUrl, pushSupport, urlBase64ToUint8Array } from '../lib/pushNotify'

afterEach(unmountAll)
before(async () => {
  await setupDom()
})
after(async () => {
  await teardownDom()
})

const text = () => document.body.textContent ?? ''

test('pushSupport：不安全環境、iPhone 未加入主畫面、不支援，各給原因', () => {
  const win = (over: Record<string, unknown>) => ({ isSecureContext: true, matchMedia: () => ({ matches: false }), PushManager: {}, Notification: {}, ...over }) as unknown as Window
  const nav = (over: Record<string, unknown>) => ({ userAgent: 'Mozilla', platform: 'Linux', maxTouchPoints: 0, serviceWorker: {}, ...over }) as unknown as Navigator
  assert.deepEqual(pushSupport({ win: win({ isSecureContext: false }), nav: nav({}) }), { ok: false, reason: 'insecure' })
  assert.deepEqual(pushSupport({ win: win({}), nav: nav({ userAgent: 'iPhone' }) }), { ok: false, reason: 'ios-home-screen' })
  assert.deepEqual(pushSupport({ win: win({}), nav: nav({ userAgent: 'iPhone', standalone: true }) }), { ok: true })
  assert.deepEqual(pushSupport({ win: win({ PushManager: undefined }), nav: nav({}) }).ok, false)
  assert.deepEqual(pushSupport({ win: win({}), nav: nav({}) }), { ok: true })
})

test('parseScheduleUrl 與 base64url 轉換', () => {
  assert.deepEqual(parseScheduleUrl('/?schedule=s1&project=p1&occurrence=o1'), { scheduleId: 's1', projectId: 'p1', occurrenceId: 'o1' })
  assert.equal(parseScheduleUrl('/?schedule=s1'), null)
  assert.deepEqual([...urlBase64ToUint8Array('AQID')], [1, 2, 3])
  assert.deepEqual([...urlBase64ToUint8Array('-_8')], [251, 255])
})

test('設定頁：不支援時顯示原因、不給啟用按鈕', async () => {
  fakeApi(() => ({}))
  // happy-dom 沒有 PushManager／serviceWorker：落在「不支援」。
  await mount(<PushSettings />)
  await settle()
  assert.match(text(), /推播通知/)
  assert.match(text(), /不支援網頁推播|HTTPS|加入主畫面/)
  assert.ok(![...document.querySelectorAll('button')].some((b) => b.textContent === '啟用推播'))
})

test('設定頁：支援且已訂閱時顯示「已啟用」，可送測試通知', async () => {
  const w = window as unknown as Record<string, unknown>
  const nav = navigator as unknown as Record<string, unknown>
  const saved = { sec: Object.getOwnPropertyDescriptor(w, 'isSecureContext'), pm: w.PushManager, no: w.Notification, sw: nav.serviceWorker }
  Object.defineProperty(w, 'isSecureContext', { value: true, configurable: true })
  w.PushManager = function PushManager() {}
  w.Notification = Object.assign(function Notification() {}, { permission: 'granted' })
  Object.defineProperty(nav, 'serviceWorker', { value: { getRegistration: async () => ({ pushManager: { getSubscription: async () => ({ endpoint: 'https://push.example/a' }) } }) }, configurable: true })
  try {
    const reqs = fakeApi((req) => {
      if (req.method === 'GET' && req.path.endsWith('/push/subscriptions')) return { subscriptions: [{ endpoint: 'https://push.example/a' }] }
      if (req.method === 'POST' && req.path.endsWith('/push/test')) return { sent: 1, removed: 0, failed: 0 }
      return {}
    })
    await mount(<PushSettings />)
    await settle()
    assert.match(text(), /已啟用/)
    await click([...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent === '送測試通知')!)
    await settle()
    assert.ok(reqs.some((r) => r.method === 'POST' && r.path.endsWith('/push/test')))
    assert.match(text(), /已送出測試通知/)
    await settle()
    await unmountAll()
  } finally {
    if (saved.sec) Object.defineProperty(w, 'isSecureContext', saved.sec)
    w.PushManager = saved.pm
    w.Notification = saved.no
    Object.defineProperty(nav, 'serviceWorker', { value: saved.sw, configurable: true })
  }
})
