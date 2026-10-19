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
/** 每顆 bot 的清單還有沒有這個檔（false＝已經被清掉）。 */
const serves: Record<string, boolean> = {}
const realFetch = globalThis.fetch
const realCreate = URL.createObjectURL
const realRevoke = URL.revokeObjectURL

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
const listBody = { dir: '/x', reason: null, ttl_secs: 3600, kept: false, files: [{ name: 'report.txt', size: 3, modified: 1, remaining_secs: 1800 }] }

interface MockWindow {
  opener: unknown
  location: { href: string }
  closed: boolean
  close: () => void
}

let mockWindow: MockWindow | null = null
let openCalledWith: { url: string; target?: string }[] = []
let shouldBlockPopup = false
const revokedUrls: string[] = []
const activeIntervals: (() => void)[] = []
const realOpen = window.open
const realSetInterval = globalThis.setInterval

before(() => {
  setupDom()
  globalThis.setInterval = (((fn: () => void, ms?: number) => {
    activeIntervals.push(fn)
    return realSetInterval(fn, ms)
  }) as unknown) as typeof setInterval
  window.open = ((url: string, target?: string) => {
    openCalledWith.push({ url, target })
    if (shouldBlockPopup) return null
    mockWindow = {
      opener: window,
      location: { href: url || '' },
      closed: false,
      close() {
        this.closed = true
      },
    }
    return mockWindow as unknown as Window
  }) as typeof window.open
  globalThis.fetch = ((input: string) => {
    const url = String(input)
    if (url.includes('/outbox/file')) {
      return new Promise<Response>((respond) => pending.push({ path: url, respond }))
    }
    // 清單：兩顆 bot 都有同名檔。立刻回。
    listHits.push(url)
    const bot = /\/bots\/(\w+)\/outbox/.exec(url)?.[1] ?? ''
    return Promise.resolve(json(serves[bot] === false ? { ...listBody, files: [] } : listBody))
  }) as typeof fetch
  URL.createObjectURL = ((b: Blob) => `blob:${b.type || 'x'}`) as typeof URL.createObjectURL
  URL.revokeObjectURL = ((url: string) => {
    revokedUrls.push(url)
  }) as typeof URL.revokeObjectURL
})

afterEach(async () => {
  await unmountAll()
  pending.length = 0
  listHits.length = 0
  openCalledWith.length = 0
  revokedUrls.length = 0
  activeIntervals.length = 0
  shouldBlockPopup = false
  mockWindow = null
  for (const k of Object.keys(serves)) delete serves[k]
})

after(async () => {
  globalThis.fetch = realFetch
  globalThis.setInterval = realSetInterval
  window.open = realOpen
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

test('舊 bot 晚回 404：通知寫明是哪顆 bot 的檔；目前 bot 的 404 仍移除那一列並發通知', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {}, notices: [] } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')
  await click(fileButton()!)
  await until(() => pending.length === 1, 'A 的下載請求送出')

  useStore.setState({ selectedBotId: 'b' } as never)
  await settle(50)
  await until(listLoaded, 'bot b 的清單載入')
  serves.a = false
  pendingFor('a').respond(json({ error: 'not_found' }, 404))
  await settle(30)
  const staleNotices = useStore.getState().notices.filter((n) => n.kind === 'error')
  assert.equal(staleNotices.length, 1, '舊 bot 的失敗要跳一則通知')
  assert.ok(staleNotices[0].text.includes('alpha'), `通知要寫明是 alpha 的檔：${staleNotices[0].text}`)

  // 目前的 bot（B）自己 404：那一列要移除，並發通知。
  serves.b = false
  await click(fileButton()!)
  await until(() => pending.length === 2, 'B 的下載請求送出')
  pendingFor('b').respond(json({ error: 'not_found' }, 404))
  await until(() => fileCount() === 0, 'B 的 404 移除那一列')
  const all = useStore.getState().notices.filter((n) => n.kind === 'error')
  assert.equal(all.length, 2, '目前 bot 的 404 也要通知')
  assert.ok(all[1].text.includes('不在了'))
  assert.equal(all[1].text.includes('alpha'), false, '目前 bot 的通知不加舊 bot 的名字')
})

test('同一顆 bot 的 404：移除那一列並發通知（#547 回歸）', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {}, notices: [] } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')
  serves.a = false
  await click(fileButton()!)
  await until(() => pending.length === 1, 'A 的下載請求送出')
  const before = listHits.filter((u) => u.includes('/bots/a/outbox')).length
  pendingFor('a').respond(json({ error: 'not_found' }, 404))
  await until(() => fileCount() === 0, '404 之後那一列要移除')
  assert.ok(listHits.filter((u) => u.includes('/bots/a/outbox')).length > before, '移除後要重讀一次清單')
  const notices = useStore.getState().notices.filter((n) => n.kind === 'error')
  assert.equal(notices.length, 1)
  assert.ok(notices[0].text.includes('不在了'))
})

test('可預覽格式（如 PDF/文字）顯示預覽按鈕，點擊同步開窗、帶 inline=1，成功後導覽至 Blob URL', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {}, notices: [] } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')

  const previewBtn = document.querySelector<HTMLButtonElement>('button.outbox-preview-btn')
  assert.ok(previewBtn, '應該要有預覽按鈕')

  await click(previewBtn)
  // 開窗先於 fetch 完成（同步開窗以保留 transient activation）
  assert.equal(openCalledWith.length, 1, '點擊當下應立即 window.open')
  assert.equal(openCalledWith[0].url, '', '應開啟空白頁')
  assert.equal(openCalledWith[0].target, '_blank', 'target 應為 _blank')
  assert.ok(mockWindow, '應取得 mockWindow handle')
  assert.equal(mockWindow.opener, null, '取得 handle 後應立即將 opener 設為 null')

  await until(() => pending.length === 1, '預覽請求送出')
  assert.ok(pending[0].path.includes('inline=1'), `請求需帶 inline=1：${pending[0].path}`)
  assert.ok(busyText().includes('開啟中…'), '預覽中狀態文字應顯示「開啟中…」')

  pending[0].respond(new Response('dummy-content', { status: 200, headers: { 'content-type': 'text/plain' } }))
  await until(() => Boolean(mockWindow?.location.href.startsWith('blob:')), '取檔完成後應導向 blob URL')
  assert.ok(mockWindow.location.href.startsWith('blob:text/plain'), `導向的網址應為 blob URL：${mockWindow.location.href}`)
})

test('window.open 回 null（彈出視窗被瀏覽器擋下）時發出通知並不送出 fetch', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {}, notices: [] } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')

  shouldBlockPopup = true
  const previewBtn = document.querySelector<HTMLButtonElement>('button.outbox-preview-btn')
  await click(previewBtn!)

  assert.equal(openCalledWith.length, 1, '有嘗試 window.open')
  assert.equal(pending.length, 0, '被擋下時不應發起 fetch 請求')
  const notices = useStore.getState().notices.filter((n) => n.kind === 'error')
  assert.equal(notices.length, 1)
  assert.equal(notices[0].text, '瀏覽器擋了彈出視窗')
})

test('取檔失敗時（如 404）會關閉分頁並發出錯誤通知', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {}, notices: [] } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')

  const previewBtn = document.querySelector<HTMLButtonElement>('button.outbox-preview-btn')
  await click(previewBtn!)
  await until(() => pending.length === 1, '預覽請求送出')
  assert.ok(mockWindow && !mockWindow.closed, '開出的分頁目前為開啟狀態')

  pending[0].respond(json({ error: 'not_found' }, 404))
  await until(() => mockWindow?.closed === true, '取檔失敗時應關閉空白分頁')
  const notices = useStore.getState().notices.filter((n) => n.kind === 'error')
  assert.equal(notices.length, 1)
  assert.ok(notices[0].text.includes('不在了'))
})

test('關窗後每 30 秒輪詢時才釋放 Blob URL', async () => {
  useStore.setState({ selectedProjectId: null, selectedBotId: 'a', bots, previews: {}, notices: [] } as never)
  await mount(<OutboxFiles />)
  await until(listLoaded, 'bot a 的清單載入')

  const previewBtn = document.querySelector<HTMLButtonElement>('button.outbox-preview-btn')
  await click(previewBtn!)
  await until(() => pending.length === 1, '預覽請求送出')
  pending[0].respond(new Response('content', { status: 200, headers: { 'content-type': 'text/plain' } }))
  await until(() => Boolean(mockWindow?.location.href.startsWith('blob:')), '取檔成功')

  const url = mockWindow!.location.href
  assert.equal(revokedUrls.length, 0, '分頁未關閉前不應 revoke')

  // 分頁未關閉時，觸發 interval 也不應 revoke
  assert.equal(activeIntervals.length > 0, true, '應註冊 interval 檢查')
  activeIntervals.forEach((fn) => fn())
  assert.equal(revokedUrls.includes(url), false, '分頁未關閉前 trigger interval 不得 revoke')

  // 關閉分頁後觸發輪詢，應釋放 Blob URL
  mockWindow!.closed = true
  activeIntervals.forEach((fn) => fn())
  assert.equal(revokedUrls.includes(url), true, '分頁關閉後應 revoke Blob URL')
})
