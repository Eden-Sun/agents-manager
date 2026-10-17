/**
 * outbox 下載中切到另一顆 bot（issue #1805）：舊 bot 的下載晚回 404，不能動目前 bot 的清單、重讀、或下載中狀態。
 * 同名檔案（`report.txt`）在兩顆 bot 底下都有，這正是會誤刪的情境。手動控制回應時機，不靠牆鐘。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { OutboxFiles } from './OutboxFiles'

interface Pending {
  path: string
  respond: (r: Response) => void
}

const pending: Pending[] = []
/** 清單的讀取（不含下載）：用來看有沒有被重讀。 */
const listHits: string[] = []
const realFetch = globalThis.fetch
const realCreate = URL.createObjectURL
const realRevoke = URL.revokeObjectURL

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
const listBody = { dir: '/x', reason: null, ttl_secs: 3600, kept: false, files: [{ name: 'report.txt', size: 3, modified: 1, remaining_secs: 1800 }] }

before(() => {
  setupDom()
  globalThis.fetch = ((input: string) => {
    const url = String(input)
    if (url.includes('/outbox/file')) {
      return new Promise<Response>((respond) => pending.push({ path: url, respond }))
    }
    // 清單：兩顆 bot 都有同名檔。立刻回。
    listHits.push(url)
    return Promise.resolve(json(listBody))
  }) as typeof fetch
  URL.createObjectURL = (() => 'blob:x') as typeof URL.createObjectURL
  URL.revokeObjectURL = (() => {}) as typeof URL.revokeObjectURL
})

afterEach(async () => {
  await unmountAll()
  pending.length = 0
  listHits.length = 0
})

after(async () => {
  globalThis.fetch = realFetch
  URL.createObjectURL = realCreate
  URL.revokeObjectURL = realRevoke
  resetStoreForTest()
  await teardownDom()
})

const bots = [
  { id: 'a', name: 'alpha', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null },
  { id: 'b', name: 'beta', parent_bot_id: null, managed_by: 'user', project_id: 'p1', kind: 'claude', identity: null },
]

const fileButton = () => document.querySelector<HTMLButtonElement>('button.outbox-file')
const fileCount = () => document.querySelectorAll('button.outbox-file').length
/** 清單是這顆 bot 的：元件的「重新讀取」只在 `loadedFor === botId` 時才可按。 */
const listLoaded = () => fileCount() === 1 && !document.querySelector<HTMLButtonElement>('.outbox-refresh')!.disabled
const busyText = () => document.querySelector('.outbox-meta')?.textContent ?? ''
const pendingFor = (botId: string) => {
  const p = pending.find((x) => x.path.includes(`/bots/${botId}/outbox/file`))
  assert.ok(p, `應該有 bot ${botId} 的下載請求`)
  return p
}

test('下載中切到另一顆 bot，舊 bot 晚回 404：新 bot 的同名檔與下載狀態都不動', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')

  // A 開始下載 report.txt，還沒回。
  await click(fileButton()!)
  await until(() => pending.length === 1, 'A 的下載請求送出')
  assert.equal(busyText().includes('下載中'), true, 'A 下載中')

  // 切到 B：B 的同名檔載入，B 自己開始下載，也還沒回。
  useStore.setState({ selectedBotId: 'b' } as never)
  // 先讓切換 render 出來：否則 `listLoaded` 還看得到 A 的清單。
  await settle(50)
  await until(listLoaded, 'bot b 的清單載入')
  await click(fileButton()!)
  await until(() => pending.length === 2, 'B 的下載請求送出')

  // A 的下載這時才回 404（檔案已被清掉）。
  pendingFor('a').respond(json({ error: 'not_found' }, 404))
  await settle(30)

  assert.equal(fileCount(), 1, 'B 的同名檔不能被舊 bot 的 404 拿掉')
  assert.ok(busyText().includes('下載中'), 'B 自己的下載狀態不能被舊請求清掉')

  // B 完成，busy 才清掉。
  pendingFor('b').respond(new Response('bee', { status: 200, headers: { 'content-type': 'text/plain' } }))
  await until(() => !busyText().includes('下載中'), 'B 下載完成後 busy 清掉')
  assert.equal(fileCount(), 1)
})

test('A 下載中切到 B，A 先完成：A 的 busy 不影響 B 的同名檔', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')
  await click(fileButton()!)
  await until(() => pending.length === 1, 'A 的下載請求送出')

  useStore.setState({ selectedBotId: 'b' } as never)
  // 先讓切換 render 出來：否則 `listLoaded` 還看得到 A 的清單。
  await settle(50)
  await until(listLoaded, 'bot b 的清單載入')
  // B 的列表此刻不該因為 A 還在下載就顯示下載中。
  assert.equal(busyText().includes('下載中'), false, 'A 的下載中不該畫在 B 的同名檔上')
  assert.equal(fileButton()!.disabled, false, 'B 的同名檔可以按')

  pendingFor('a').respond(new Response('ay', { status: 200, headers: { 'content-type': 'text/plain' } }))
  await settle(30)
  assert.equal(fileCount(), 1, 'A 成功完成也不動 B 的清單')
})

test('A 下載中切到 B、B 還沒下載，A 晚回 404：B 的同名檔清單不被移除、不重讀', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {} } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')
  await click(fileButton()!)
  await until(() => pending.length === 1, 'A 的下載請求送出')

  useStore.setState({ selectedBotId: 'b' } as never)
  await settle(50)
  await until(listLoaded, 'bot b 的清單載入')

  const bListsBefore = listHits.filter((u) => u.includes('/bots/b/outbox')).length
  pendingFor('a').respond(json({ error: 'not_found' }, 404))
  await settle(30)
  assert.equal(fileCount(), 1, 'B 的同名檔不能被 A 的 404 移除')
  assert.equal(listHits.filter((u) => u.includes('/bots/b/outbox')).length, bListsBefore, 'B 的清單不能因為 A 的 404 被重讀')
})
