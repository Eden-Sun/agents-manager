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
    if (/\/bots\/[^/]+\/restart(\?|$)/.test(url)) {
      calls.push({ method, url, body })
      return new Response(JSON.stringify({ run_id: 'r2', resumed: true }), { status: 200, headers: { 'content-type': 'application/json' } })
    }
    if (url.includes('/bots/restart-idle')) {
      calls.push({ method, url, body })
      return new Response(JSON.stringify({ batch_id: 'b1', total: 1, planned: [{ bot_id: 'ready', name: 'ready' }], skipped: [] }), { status: 200, headers: { 'content-type': 'application/json' } })
    }
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
  assert.match(chip()!.title, /a · 預設 · 正在跑/)
})

test('全忙時打開的確認框列出忙碌名單，按「閒下來自動重啟」呼叫 API，不重啟任何一顆', async () => {
  useStore.setState({ bots: [bot('a'), bot('b')], runs: { a: working('a'), b: working('b') }, busy: {} })
  await open()
  const items = [...document.querySelectorAll('.confirm-list.dim li .confirm-row-text')].map((li) => li.textContent ?? '')
  assert.deepEqual(items, ['a · 預設 · 正在跑', 'b · 預設 · 正在跑'], '名單含專案／身份與各自在做什麼')
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
  assert.match(document.querySelector('.confirm-list.dim li .confirm-row-text')!.textContent ?? '', /· 已排程$/)
  await click(buttonByText('取消自動重啟')!)
  await until(() => calls.some((c) => c.method === 'DELETE'), '送出取消')
  assert.deepEqual(calls.find((c) => c.method === 'DELETE')!.body, { bot_ids: ['a'] })
  await until(() => useStore.getState().updateRestartScheduled.length === 0, 'store 清掉')
})

test('有閒置的也有忙的：一次重啟閒置的，並自動把忙的排成閒下來自動重啟', async () => {
  useStore.setState({ bots: [bot('ready'), bot('busy1')], runs: { ready: run('ready'), busy1: working('busy1') }, busy: {} })
  await open()
  assert.equal(document.querySelector('.confirm-check'), null, '不再有要勾的選項')
  await click(buttonByText('重啟 1 顆')!)
  await until(() => calls.some((c) => c.url.includes('update-restart')), '自動排程')
  assert.ok(calls.some((c) => c.url.includes('restart-idle')), '閒置的重啟')
  assert.deepEqual(calls.find((c) => c.url.includes('update-restart'))!.body, { bot_ids: ['busy1'] })
})

const restartCalls = () => calls.filter((c) => /\/bots\/[^/]+\/restart/.test(c.url) && !c.url.includes('restart-idle'))
const forceButtons = () => [...document.querySelectorAll<HTMLButtonElement>('button.confirm-row-force:not(.all)')]

test('每列（含在忙的）有「立即重啟」；按下要二次確認，確認文字寫清楚會中斷回合，確認後才呼叫單顆 restart', async () => {
  useStore.setState({ bots: [bot('ready'), bot('busy1')], runs: { ready: run('ready'), busy1: working('busy1') }, busy: {} })
  await open()
  assert.equal(forceButtons().length, 2, '閒置與在忙的都有按鈕')
  const busyBtn = forceButtons()[1]
  await click(busyBtn)
  await settle()
  assert.equal(restartCalls().length, 0, '第一下只開確認框，不直接重啟')
  const text = document.body.textContent ?? ''
  assert.match(text, /會中斷這顆目前的回合（Esc 停下），之後用 --resume 接回，上下文不會掉，但正在進行的工作要重新下指令/)
  await click(buttonByText('立即重啟 1 顆')!)
  await until(() => restartCalls().length === 1, '確認後送出 restart')
  assert.match(restartCalls()[0].url, /\/bots\/busy1\/restart/)
  assert.equal(restartCalls()[0].method, 'POST')
})

test('二次確認按取消：不重啟、回到名單', async () => {
  useStore.setState({ bots: [bot('busy1')], runs: { busy1: working('busy1') }, busy: {} })
  await open()
  await click(forceButtons()[0])
  await settle()
  await click(buttonByText('取消')!)
  await settle()
  assert.equal(restartCalls().length, 0)
  assert.ok(document.querySelector('.confirm-list'), '回到原本的名單')
})

test('「全部立即重啟」同樣二次確認，確認後一顆顆呼叫 restart', async () => {
  useStore.setState({ bots: [bot('ready'), bot('busy1')], runs: { ready: run('ready'), busy1: working('busy1') }, busy: {} })
  await open()
  await click(buttonByText('全部立即重啟')!)
  await settle()
  assert.equal(restartCalls().length, 0)
  assert.match(document.body.textContent ?? '', /會中斷這 2 顆目前的回合/)
  await click(buttonByText('立即重啟 2 顆')!)
  await until(() => restartCalls().length === 2, '兩顆都送 restart')
  assert.deepEqual(restartCalls().map((c) => c.url.match(/bots\/([^/]+)\/restart/)![1]).sort(), ['busy1', 'ready'])
})

test('子 agent 不出現在名單、也沒有立即重啟按鈕', async () => {
  useStore.setState({
    bots: [bot('kid', { managed_by: 'child', parent_bot_id: 'p' }), bot('busy1')],
    runs: { kid: working('kid'), busy1: working('busy1') },
    busy: {},
  })
  await open()
  const rows = [...document.querySelectorAll('.confirm-row-text')].map((e) => e.textContent ?? '')
  assert.ok(!rows.some((r) => r.startsWith('kid')), `子 agent 不列：${rows}`)
  assert.equal(forceButtons().length, 1)
})
