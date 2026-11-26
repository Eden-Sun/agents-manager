import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { OutboxFiles } from './OutboxFiles'

const realFetch = globalThis.fetch
const realSetInterval = globalThis.setInterval
const realClearInterval = globalThis.clearInterval
let mode: 'empty' | 'failed' | 'files' = 'empty'
const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } })

before(async () => {
  await setupDom()
  globalThis.fetch = (() => {
    if (mode === 'failed') return Promise.reject(new Error('network down'))
    if (mode === 'files') {
      return Promise.resolve(
        json({
          dir: '/tmp/outbox-a',
          reason: null,
          ttl_secs: 3600,
          kept: false,
          files: [
            { name: '車險報價-本次更新重點.pdf', size: 1_200_000, modified: 2, remaining_secs: 3500 },
            { name: 'README', size: 500, modified: 1, remaining_secs: 3500 },
          ],
        }),
      )
    }
    return Promise.resolve(json({ dir: '/tmp/outbox-a', reason: null, ttl_secs: 3600, kept: false, files: [] }))
  }) as typeof fetch
  globalThis.setInterval = ((() => 1) as unknown) as typeof setInterval
  globalThis.clearInterval = (() => {}) as typeof clearInterval
})

afterEach(async () => {
  await unmountAll()
  mode = 'empty'
})

after(async () => {
  globalThis.fetch = realFetch
  globalThis.setInterval = realSetInterval
  globalThis.clearInterval = realClearInterval
  resetStoreForTest()
  await teardownDom()
})

const bots = [{ id: 'a', name: 'alpha', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null }]

test('正常無檔時收斂成單列，不畫三行說明段落，完整說明留在 title', async () => {
  mode = 'empty'
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelector('.outbox-empty-inline') !== null, '單列無檔案')
  assert.ok(document.querySelector('.outbox-files.compact'))
  assert.equal(document.querySelector('.outbox-empty'), null)
  assert.equal(document.querySelector('.outbox-empty-inline')!.textContent, '無檔案')
  assert.match(document.querySelector<HTMLElement>('.outbox-title')!.title, /AM_OUTBOX/)
  assert.match(document.querySelector<HTMLElement>('.outbox-title')!.title, /\/tmp\/outbox-a/)
})

test('讀取失敗仍展開警告段落，不收成無檔案', async () => {
  mode = 'failed'
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelector('.outbox-empty.failed') !== null, '錯誤提示')
  assert.equal(document.querySelector('.outbox-empty-inline'), null)
  assert.equal(document.querySelector('.outbox-empty.failed')!.getAttribute('role'), 'alert')
})

test('有檔案時第一行顯示主檔名、第二行顯示副檔名標籤，title 保留完整檔名；無副檔名不顯示標籤', async () => {
  mode = 'files'
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(() => document.querySelectorAll('.outbox-item').length === 2, '兩筆檔案載入')
  const items = [...document.querySelectorAll('.outbox-item')]
  assert.equal(items[0].querySelector('.outbox-name')!.textContent, '車險報價-本次更新重點')
  assert.equal(items[0].querySelector('.outbox-ext')!.textContent, 'PDF')
  assert.ok(items[0].querySelector('.outbox-ext.outbox-ext-pdf'))
  assert.match(items[0].querySelector<HTMLButtonElement>('.outbox-file')!.title, /車險報價-本次更新重點\.pdf/)
  assert.equal(items[1].querySelector('.outbox-name')!.textContent, 'README')
  assert.equal(items[1].querySelector('.outbox-ext'), null)
})
