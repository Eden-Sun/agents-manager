/**
 * 更新晶片「有更新但相關 Bot 都在忙」（使用者 2026-11-18：「這個更新無法點，不合理」）：晶片永遠可點，
 * 全忙時開同一個確認框，列出更新內容與忙碌名單，可「閒下來自動重啟」（不強制重啟）或取消。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run } from '../api/types'
import { RestartChip } from './UpdateQuotaChip'

const realFetch = globalThis.fetch
const calls: { method: string; url: string; body: unknown }[] = []
let serverIds: string[] = []

before(async () => {
  await setupDom()
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    if (url.includes('/bots/update-restart')) {
      calls.push({ method, url, body })
      if (method === 'POST') serverIds = [...new Set([...serverIds, ...(body.bot_ids as string[])])]
      if (method === 'DELETE') serverIds = (body.bot_ids as string[]).length === 0 ? [] : serverIds.filter((x) => !(body.bot_ids as string[]).includes(x))
      return new Response(JSON.stringify({ bot_ids: serverIds }), { status: 200, headers: { 'content-type': 'application/json' } })
    }
    if (url.includes('/bots/restart-idle')) {
      calls.push({ method, url, body })
      return new Response(JSON.stringify({ batch_id: 'b1', total: 1, planned: [{ bot_id: 'ready', name: 'ready' }], skipped: [] }), { status: 200, headers: { 'content-type': 'application/json' } })
    }
    // changelog 等其他請求：空回應即可。
    return new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } })
  }) as typeof fetch
})
afterEach(async () => {
  await unmountAll()
  calls.length = 0
  serverIds = []
  useStore.setState({ bots: [], runs: {}, busy: {}, updateRestartScheduled: [], restartBatch: null })
})
after(async () => {
  globalThis.fetch = realFetch
  resetStoreForTest()
  await teardownDom()
})

const bot = (id: string, over: Partial<Bot> = {}): Bot =>
  ({ id, name: id, project_id: 'p1', kind: 'claude', managed_by: 'user', ...over }) as unknown as Bot
const run = (botId: string, over: Partial<Run> = {}): Run =>
  ({ id: `r-${botId}`, bot_id: botId, state: 'running', agent_status: 'idle', update_notice: 'Update installed · Restart to update', background_jobs: 0, ...over }) as unknown as Run
const working = (botId: string): Run => run(botId, { agent_status: 'working' })

const chip = () => document.querySelector<HTMLButtonElement>('button.quota-update')
const buttonByText = (text: string) => [...document.querySelectorAll('button')].find((b) => b.textContent === text)

async function open() {
  await mount(<RestartChip />)
  await until(() => chip() !== null, '晶片畫出來')
  await click(chip()!)
  await until(() => document.querySelector('.confirm-list') !== null, '確認框畫出來')
}

test('全忙時晶片可點、標示在忙數，title 說明可安排自動重啟', async () => {
  useStore.setState({ bots: [bot('a'), bot('b')], runs: { a: working('a'), b: working('b') }, busy: {} })
  await mount(<RestartChip />)
  await until(() => chip() !== null, '晶片畫出來')
  assert.equal(chip()!.disabled, false, '全忙也不能是 disabled')
  assert.equal(chip()!.querySelector('.quota-update-n')!.textContent, '2')
  assert.match(chip()!.getAttribute('aria-label') ?? '', /2 顆在忙，點開可安排閒下來自動重啟/)
  assert.match(chip()!.title, /a（/)
})

test('全忙時打開的確認框列出忙碌名單，按「閒下來自動重啟」呼叫 API，不重啟任何一顆', async () => {
  useStore.setState({ bots: [bot('a'), bot('b')], runs: { a: working('a'), b: working('b') }, busy: {} })
  await open()
  const items = [...document.querySelectorAll('.confirm-list.dim li')].map((li) => li.textContent ?? '')
  assert.equal(items.length, 2)
  assert.ok(items[0].startsWith('a（') && items[1].startsWith('b（'), `名單含各自在做什麼：${items}`)
  assert.ok(buttonByText('閒下來自動重啟'))
  assert.equal(buttonByText('重啟 0 顆'), undefined, '沒有強制重啟')
  await click(buttonByText('閒下來自動重啟')!)
  await until(() => calls.some((c) => c.url.includes('update-restart')), '送出排程')
  assert.deepEqual(calls.find((c) => c.url.includes('update-restart'))!.body, { bot_ids: ['a', 'b'] })
  assert.ok(!calls.some((c) => c.url.includes('restart-idle')), '不呼叫一鍵重啟')
  await until(() => useStore.getState().updateRestartScheduled.length === 2, 'store 記下排程')
})

test('已排程：晶片有時鐘角標，確認框改成「取消自動重啟」並標已排程', async () => {
  useStore.setState({ bots: [bot('a')], runs: { a: working('a') }, busy: {}, updateRestartScheduled: ['a'] })
  serverIds = ['a']
  await mount(<RestartChip />)
  await until(() => chip() !== null, '晶片畫出來')
  assert.ok(chip()!.classList.contains('scheduled'))
  assert.ok(chip()!.querySelector('.quota-update-clock'), '時鐘角標')
  assert.match(chip()!.getAttribute('aria-label') ?? '', /已排程/)
  await click(chip()!)
  await until(() => document.querySelector('.confirm-list') !== null, '確認框')
  assert.match(document.querySelector('.confirm-list.dim li')!.textContent ?? '', /（已排程）/)
  await click(buttonByText('取消自動重啟')!)
  await until(() => calls.some((c) => c.method === 'DELETE'), '送出取消')
  assert.deepEqual(calls.find((c) => c.method === 'DELETE')!.body, { bot_ids: ['a'] })
  await until(() => useStore.getState().updateRestartScheduled.length === 0, 'store 清掉')
})

test('有閒置的也有忙的：照舊重啟閒置的，勾選才把其餘排成自動重啟', async () => {
  useStore.setState({ bots: [bot('ready'), bot('busy1')], runs: { ready: run('ready'), busy1: working('busy1') }, busy: {} })
  await open()
  const box = document.querySelector<HTMLInputElement>('.confirm-check input')!
  assert.ok(box, '有「其餘閒下來也自動重啟」勾選')
  await click(box)
  await settle()
  await click(buttonByText('重啟 1 顆')!)
  await until(() => calls.some((c) => c.url.includes('update-restart')), '勾了才排程')
  assert.ok(calls.some((c) => c.url.includes('restart-idle')), '閒置的照舊重啟')
  assert.deepEqual(calls.find((c) => c.url.includes('update-restart'))!.body, { bot_ids: ['busy1'] })
})

test('有閒置的也有忙的、沒勾選：不排程', async () => {
  useStore.setState({ bots: [bot('ready'), bot('busy1')], runs: { ready: run('ready'), busy1: working('busy1') }, busy: {} })
  await open()
  await click(buttonByText('重啟 1 顆')!)
  await until(() => calls.some((c) => c.url.includes('restart-idle')), '重啟')
  await settle()
  assert.ok(!calls.some((c) => c.method === 'POST' && c.url.includes('update-restart')))
})
