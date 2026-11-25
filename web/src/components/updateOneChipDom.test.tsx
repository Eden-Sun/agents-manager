import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { handleFrame, resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run } from '../api/types'
import { UpdateQuotaChip } from './UpdateQuotaChip'

const realFetch = globalThis.fetch
const calls: { method: string; url: string; body: unknown }[] = []

before(async () => {
  await setupDom()
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    const json = (v: unknown) => new Response(JSON.stringify(v), { status: 200, headers: { 'content-type': 'application/json' } })
    if (url.includes('/update-chain')) {
      calls.push({ method, url, body })
      return json({ chain_id: 'c1', started: [{ update_id: 'u1', host: 'local', kind: 'claude' }], rejected: [] })
    }
    if (url.includes('/bots/restart-idle')) {
      calls.push({ method, url, body })
      return json({ batch_id: 'b1', total: 1, planned: [{ bot_id: 'ready', name: 'ready' }], skipped: [] })
    }
    if (url.includes('/bots/update-restart')) {
      if (method === 'GET') return json({ bot_ids: [] })
      calls.push({ method, url, body })
      return json({ bot_ids: (body as { bot_ids: string[] }).bot_ids })
    }
    return json({})
  }) as typeof fetch
})
afterEach(async () => {
  await unmountAll()
  calls.length = 0
  useStore.setState({ bots: [], runs: {}, busy: {}, updateRestartScheduled: [], restartBatch: null, cliUpdates: [], upstreamUpdates: {}, updateChain: false, refreshState: realRefresh })
})
after(async () => {
  globalThis.fetch = realFetch
  resetStoreForTest()
  await teardownDom()
})

const bot = (id: string): Bot => ({ id, name: id, project_id: 'p1', kind: 'claude', managed_by: 'user' }) as unknown as Bot
const run = (botId: string, status = 'idle'): Run =>
  ({ id: `r-${botId}`, bot_id: botId, state: 'running', agent_status: status, update_notice: 'Update installed · Restart to update', background_jobs: 0 }) as unknown as Run
const claudeUpstream = {
  kind: 'claude', latest: '2.0.0', target: '2.0.0', hasUpdate: true, notify: null, text: null,
  hosts: [{ host: 'local', installedVersion: '1.9.0', error: null, behind: true }],
}

const realRefresh = useStore.getState().refreshState
const chips = () => [...document.querySelectorAll<HTMLButtonElement>('button.quota-update')]
const buttonByText = (text: string) => [...document.querySelectorAll('button')].find((b) => b.textContent === text)

function seed() {
  useStore.setState({
    refreshState: async () => {},
    bots: [bot('ready'), bot('busy1')],
    runs: { ready: run('ready'), busy1: run('busy1', 'working') },
    upstreamUpdates: { claude: claudeUpstream },
  })
}

test('同一次 claude 升級只有一個晶片，數字是安裝＋重啟＋在忙的總待辦，tooltip 分列', async () => {
  seed()
  await mount(<UpdateQuotaChip />)
  await until(() => chips().length > 0, '晶片畫出來')
  assert.equal(chips().length, 1, '一次升級只一顆')
  assert.equal(chips()[0].querySelector('.quota-update-n')!.textContent, '3')
  const tip = chips()[0].title
  assert.match(tip, /local：安裝 claude 2\.0\.0/)
  assert.match(tip, /要重啟 1 顆：ready/)
  assert.match(tip, /1 顆在忙/)
})

test('點一下確認：只送一個串接請求，安裝→重啟→排程由 daemon 做，網頁不再自己重啟或排程', async () => {
  seed()
  await mount(<UpdateQuotaChip />)
  await until(() => chips().length === 1, '晶片')
  await click(chips()[0])
  await until(() => buttonByText('安裝並重啟') !== undefined, '站內確認框')
  await click(buttonByText('安裝並重啟')!)
  await until(() => calls.some((c) => c.url.includes('/update-chain')), '送出串接')
  assert.deepEqual(calls[0].body, { installs: [{ host: 'local', kind: 'claude', target_version: '2.0.0' }] })
  assert.equal(chips().length, 1, '安裝中仍只有一個晶片')
  handleFrame(useStore.setState, useStore.getState, { type: 'cli_update_done', data: { update_id: 'u1', host: 'local', kind: 'claude', ok: true, to: '2.0.0', restart_status: 'chained' } })
  handleFrame(useStore.setState, useStore.getState, {
    type: 'update_chain_done',
    data: { chain_id: 'c1', ok: true, restarts: [{ batch_id: 'b1', total: 1, planned: [{ bot_id: 'ready', name: 'ready' }], skipped: [] }], scheduled: ['busy1'], errors: [] },
  })
  await settle()
  assert.equal(useStore.getState().updateChain, false)
  assert.equal(useStore.getState().restartBatch?.id, 'b1', '重啟進度接 daemon 開的那一批')
  assert.deepEqual(calls.map((c) => c.url), ['/api/update-chain'], '網頁沒有另外呼叫 restart-idle 或 update-restart')
})

test('串接失敗：只收到失敗事件，不重啟也不排程', async () => {
  seed()
  await mount(<UpdateQuotaChip />)
  await until(() => chips().length === 1, '晶片')
  await click(chips()[0])
  await until(() => buttonByText('安裝並重啟') !== undefined, '確認框')
  await click(buttonByText('安裝並重啟')!)
  await until(() => calls.some((c) => c.url.includes('/update-chain')), '送出串接')
  handleFrame(useStore.setState, useStore.getState, { type: 'cli_update_done', data: { update_id: 'u1', host: 'local', kind: 'claude', ok: false, reason: 'install_failed' } })
  handleFrame(useStore.setState, useStore.getState, { type: 'update_chain_done', data: { chain_id: 'c1', ok: false, reason: 'install_failed', failed: [{ host: 'local', kind: 'claude', error: 'boom' }] } })
  await settle()
  assert.deepEqual(calls.map((c) => c.method + ' ' + c.url), ['POST /api/update-chain'], '只有串接那一下')
  assert.equal(useStore.getState().updateChain, false)
  assert.equal(useStore.getState().restartBatch, null)
})

test('網頁中途重載：daemon 還有串接在跑時晶片保持執行中，收尾後恢復', async () => {
  useStore.setState({ updateChain: false })
  handleFrame(useStore.setState, useStore.getState, { type: 'update_chain_progress', data: { chain_id: 'c1', phase: 'installing' } })
  assert.equal(useStore.getState().updateChain, true)
  handleFrame(useStore.setState, useStore.getState, { type: 'update_chain_done', data: { chain_id: 'c1', ok: true, restarts: [], scheduled: [], errors: [] } })
  assert.equal(useStore.getState().updateChain, false)
})

test('只剩重啟待辦（已安裝）：同一顆晶片只講重啟，不提安裝', async () => {
  useStore.setState({ bots: [bot('ready')], runs: { ready: run('ready') } })
  await mount(<UpdateQuotaChip />)
  await until(() => chips().length === 1, '晶片')
  assert.equal(chips()[0].querySelector('.quota-update-n')!.textContent, '1')
  assert.doesNotMatch(chips()[0].title, /安裝/)
})

test('什麼都不用做時晶片消失', async () => {
  await mount(<UpdateQuotaChip />)
  await settle()
  assert.equal(chips().length, 0)
})
