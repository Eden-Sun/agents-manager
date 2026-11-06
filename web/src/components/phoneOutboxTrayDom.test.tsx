/**
 * 手機「bot 給的檔案」收合列（使用者 2026-11-03）：有檔案才出現、預設收合、點開看得到圖檔縮圖、點縮圖開全螢幕預覽；
 * 沒有檔案整條不畫；桌面不掛。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { clearOutboxPreviews } from '../lib/outboxPreviewCache'
import { PhoneOutboxTray } from './PhoneOutboxTray'

const realFetch = globalThis.fetch
const realCreate = URL.createObjectURL
let realMatchMedia: typeof window.matchMedia
let phone = true
/** 每顆 bot 的 outbox 清單。 */
const served: Record<string, { name: string; size: number; modified: number; remaining_secs: number }[]> = {}

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } })

before(async () => {
  await setupDom()
  realMatchMedia = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: phone && q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  globalThis.fetch = ((input: string) => {
    const url = String(input)
    if (url.includes('/outbox/file')) return Promise.resolve(new Response(new Blob(['png'], { type: 'image/png' }), { status: 200 }))
    const bot = /\/bots\/(\w+)\/outbox/.exec(url)?.[1] ?? ''
    return Promise.resolve(json({ dir: '/x', reason: null, ttl_secs: 3600, kept: false, files: served[bot] ?? [] }))
  }) as typeof fetch
  URL.createObjectURL = ((b: Blob) => `blob:${b.type || 'x'}`) as typeof URL.createObjectURL
})

afterEach(async () => {
  await unmountAll()
  await settle()
  clearOutboxPreviews()
  phone = true
  try {
    localStorage.clear()
  } catch {}
  for (const k of Object.keys(served)) delete served[k]
})

after(async () => {
  globalThis.fetch = realFetch
  URL.createObjectURL = realCreate
  window.matchMedia = realMatchMedia
  resetStoreForTest()
  await teardownDom()
})

const bots = [{ id: 'a', name: 'alpha', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null }]
const file = (name: string) => ({ name, size: 2048, modified: 1, remaining_secs: 1800 })
const bar = () => document.querySelector<HTMLButtonElement>('.phone-outbox-bar')

test('有檔案：輸入框上方出現收合列，預設收合；點開出現縮圖，點縮圖開全螢幕預覽', async () => {
  served.a = [file('shot-1.png'), file('shot-2.png'), file('report.md')]
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots } as never)
  await mount(<PhoneOutboxTray />)
  await until(() => bar() !== null, '收合列出現')
  assert.match(bar()!.textContent ?? '', /bot 給的檔案（3）/)
  assert.equal(bar()!.getAttribute('aria-expanded'), 'false', '預設收合')
  assert.equal(document.querySelector('.phone-outbox-thumbs'), null, '收合時不畫縮圖')
  assert.equal(document.querySelector('.phone-outbox-new'), null, '第一次讀到的檔不算新')

  await click(bar()!)
  assert.equal(bar()!.getAttribute('aria-expanded'), 'true')
  assert.equal(document.querySelectorAll('.phone-outbox-thumb').length, 2, '兩張圖各一個縮圖，md 沒有')
  await until(() => document.querySelectorAll('img.phone-outbox-thumb-img').length === 2, '縮圖載入成 blob')
  assert.equal(document.querySelectorAll('.phone-outbox-item').length, 1, '圖檔不列檔名，清單只剩 md')
  assert.equal(localStorage.getItem('am.phoneOutboxTray.open'), '1', '展開狀態記住')

  await click(document.querySelector<HTMLButtonElement>('.phone-outbox-thumb')!)
  await until(() => document.querySelector('.image-viewer img') !== null, '全螢幕預覽的圖')
  assert.ok(document.querySelector('.image-viewer .image-viewer-stage'), '開了預覽')
})

test('沒有檔案：整條不畫', async () => {
  served.a = []
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots } as never)
  await mount(<PhoneOutboxTray />)
  await settle()
  await settle()
  assert.equal(document.querySelector('.phone-outbox'), null)
})

test('桌面：不掛（outbox 在右側欄托盤）', async () => {
  phone = false
  served.a = [file('shot-1.png')]
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots } as never)
  await mount(<PhoneOutboxTray />)
  await settle()
  await settle()
  assert.equal(document.querySelector('.phone-outbox'), null)
})
