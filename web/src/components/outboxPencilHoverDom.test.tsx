import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { OutboxFiles } from './OutboxFiles'

const realFetch = globalThis.fetch
const realSetInterval = globalThis.setInterval
const realClearInterval = globalThis.clearInterval
const realCreateObjectURL = URL.createObjectURL
const realRevokeObjectURL = URL.revokeObjectURL
const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } })

before(async () => {
  await setupDom()
  URL.createObjectURL = () => 'blob:mock-preview'
  URL.revokeObjectURL = () => {}
  globalThis.fetch = ((input: RequestInfo | URL) => {
    const url = String(input)
    if (url.includes('/outbox/file')) {
      return Promise.resolve(new Response(new Uint8Array([137, 80, 78, 71]), { status: 200, headers: { 'content-type': 'image/png' } }))
    }
    return Promise.resolve(
      json({
        dir: '/tmp/outbox-a',
        reason: null,
        ttl_secs: 3600,
        kept: false,
        files: [
          { name: 'shot.png', size: 120_000, modified: 10, remaining_secs: 3500 },
          { name: 'notes.pdf', size: 50_000, modified: 9, remaining_secs: 3500 },
        ],
      }),
    )
  }) as typeof fetch
  globalThis.setInterval = ((() => 1) as unknown) as typeof setInterval
  globalThis.clearInterval = (() => {}) as typeof clearInterval
})

afterEach(async () => {
  await unmountAll()
})

after(async () => {
  globalThis.fetch = realFetch
  globalThis.setInterval = realSetInterval
  globalThis.clearInterval = realClearInterval
  URL.createObjectURL = realCreateObjectURL
  URL.revokeObjectURL = realRevokeObjectURL
  resetStoreForTest()
  await teardownDom()
})

const bots = [{ id: 'a', name: 'alpha', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null }]
const ptr = (type: string, pointerType: 'mouse' | 'pen' | 'touch') =>
  new PointerEvent(type, { bubbles: type !== 'pointerenter' && type !== 'pointerleave', pointerType, isPrimary: true, pointerId: 1 })

async function mountAndGetImageBtn(): Promise<HTMLButtonElement> {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelectorAll('.outbox-file').length === 2, '檔案清單載入')
  return document.querySelectorAll<HTMLButtonElement>('.outbox-file')[0]
}

test('Apple Pencil（pen）懸停圖片檔會顯示預覽，移開即隱藏', async () => {
  const btn = await mountAndGetImageBtn()
  btn.dispatchEvent(ptr('pointerover', 'pen'))
  await until(() => document.querySelector('.outbox-preview img') !== null, 'pen hover 顯示圖片預覽', 1000)
  btn.dispatchEvent(ptr('pointerout', 'pen'))
  await until(() => document.querySelector('.outbox-preview') === null, 'pen leave 關閉預覽', 500)
})

test('Apple Pencil 僅觸發連續 pointermove 時不重置計時器仍能顯示預覽，pointercancel 關閉', async () => {
  const btn = await mountAndGetImageBtn()
  btn.dispatchEvent(ptr('pointermove', 'pen'))
  await new Promise((r) => setTimeout(r, 90))
  btn.dispatchEvent(ptr('pointermove', 'pen'))
  await until(() => document.querySelector('.outbox-preview img') !== null, 'pen pointermove 顯示預覽', 1000)
  btn.dispatchEvent(ptr('pointercancel', 'pen'))
  await until(() => document.querySelector('.outbox-preview') === null, 'pointercancel 關閉預覽', 500)
})

test('滑鼠（mouse）懸停顯示預覽，手指觸控（touch）不觸發 hover 預覽', async () => {
  const btn = await mountAndGetImageBtn()
  btn.dispatchEvent(ptr('pointerover', 'touch'))
  btn.dispatchEvent(ptr('pointermove', 'touch'))
  await new Promise((r) => setTimeout(r, 220))
  assert.equal(document.querySelector('.outbox-preview'), null)

  btn.dispatchEvent(ptr('pointerover', 'mouse'))
  await until(() => document.querySelector('.outbox-preview img') !== null, 'mouse hover 顯示預覽', 1000)
  btn.dispatchEvent(ptr('pointerout', 'mouse'))
  await until(() => document.querySelector('.outbox-preview') === null, 'mouse leave 關閉預覽', 500)
})
