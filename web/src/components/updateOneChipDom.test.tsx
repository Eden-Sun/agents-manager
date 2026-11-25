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
    if (url.includes('/cli-update')) {
      calls.push({ method, url, body })
      return json({ update_id: 'u1' })
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

test('點一下確認：先安裝，裝完才自動重啟閒置的，再把忙的排成閒下來自動重啟', async () => {
  seed()
  await mount(<UpdateQuotaChip />)
  await until(() => chips().length === 1, '晶片')
  await click(chips()[0])
  await until(() => buttonByText('安裝並重啟') !== undefined, '站內確認框')
  await click(buttonByText('安裝並重啟')!)
  await until(() => calls.some((c) => c.url.includes('/cli-update')), '送出安裝')
  assert.deepEqual(calls[0].body, { kind: 'claude', target_version: '2.0.0' })
  assert.ok(!calls.some((c) => c.url.includes('restart-idle')), '裝完前不重啟')
  assert.equal(chips().length, 1, '安裝中仍只有一個晶片')
  handleFrame(useStore.setState, useStore.getState, { type: 'cli_update_done', data: { update_id: 'u1', host: 'local', kind: 'claude', ok: true, to: '2.0.0', restart_required: true } })
  await until(() => calls.some((c) => c.url.includes('update-restart')), '忙的排程')
  const order = calls.map((c) => (c.url.includes('/cli-update') ? 'install' : c.url.includes('restart-idle') ? 'restart' : 'schedule'))
  assert.deepEqual(order, ['install', 'restart', 'schedule'])
  assert.deepEqual(calls[2].body, { bot_ids: ['busy1'] })
})

test('安裝失敗：不重啟也不排程', async () => {
  seed()
  await mount(<UpdateQuotaChip />)
  await until(() => chips().length === 1, '晶片')
  await click(chips()[0])
  await until(() => buttonByText('安裝並重啟') !== undefined, '確認框')
  await click(buttonByText('安裝並重啟')!)
  await until(() => calls.some((c) => c.url.includes('/cli-update')), '送出安裝')
  handleFrame(useStore.setState, useStore.getState, { type: 'cli_update_done', data: { update_id: 'u1', host: 'local', kind: 'claude', ok: false, reason: 'install_failed' } })
  await settle()
  assert.deepEqual(calls.map((c) => c.method + ' ' + c.url), ['POST /api/hosts/local/cli-update'], '只有安裝那一下')
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
