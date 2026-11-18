/**
 * 一鍵重啟確認框的名單（#998）：名字可以含「、」，名單不能先用「、」「\n」串成一個字串、畫的時候再拆回來——
 * 名字含「、」會被拆成兩列；兩顆同名的 bot 用名字當 React key 會 duplicate key。真的掛 `RestartChip` 進 happy-dom，
 * 開確認框，數畫出來的 `<li>`。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run } from '../api/types'
import { RestartChip } from './UpdateQuotaChip'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  useStore.setState({ bots: [], runs: {} })
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const bot = (id: string, over: Partial<Bot> = {}): Bot =>
  ({ id, name: id, project_id: 'p1', kind: 'claude', managed_by: 'user', ...over }) as unknown as Bot

const run = (botId: string, over: Partial<Run> = {}): Run =>
  ({
    id: `r-${botId}`,
    bot_id: botId,
    state: 'running',
    agent_status: 'idle',
    update_notice: 'Update installed · Restart to update',
    background_jobs: 0,
    ...over,
  }) as unknown as Run

const readyItems = () => [...document.querySelectorAll<HTMLLIElement>('.confirm-list:not(.dim) li .confirm-row-text')].map((li) => li.textContent)
const busyItems = () => [...document.querySelectorAll<HTMLLIElement>('.confirm-list.dim li .confirm-row-text')].map((li) => li.textContent)

async function openConfirm() {
  await mount(<RestartChip />)
  await until(() => document.querySelector('button.quota-update') !== null, '重啟鈕畫出來')
  await click(document.querySelector('button.quota-update')!)
  await until(() => document.querySelector('.confirm-list') !== null, '確認框的名單畫出來')
}

test('名字含「、」的 bot 在確認框裡是一列，不被拆成兩列', async () => {
  useStore.setState({ bots: [bot('b1', { name: '前端、後端' })], runs: { b1: run('b1') }, busy: {} })
  await openConfirm()
  assert.deepEqual(readyItems(), ['前端、後端 · 預設'])
})

test('兩顆同名 bot（不同專案）各一列，沒有 duplicate key', async () => {
  const errors: unknown[][] = []
  const original = console.error
  console.error = (...args: unknown[]) => {
    errors.push(args)
  }
  try {
    useStore.setState({
      bots: [bot('r1', { name: 'review', project_id: 'p1' }), bot('r2', { name: 'review', project_id: 'p2' })],
      runs: { r1: run('r1'), r2: run('r2') },
      busy: {},
    })
    await openConfirm()
    assert.deepEqual(readyItems(), ['review · 預設', 'review · 預設'])
  } finally {
    console.error = original
  }
  const dup = errors.filter((args) => args.some((a) => String(a).includes('same key')))
  assert.deepEqual(dup, [], `不該有 duplicate key 警告：${JSON.stringify(dup)}`)
})

test('在忙的那份照舊：進「會跳過」名單、一列一顆、寫正在跑', async () => {
  useStore.setState({
    bots: [bot('ok'), bot('busy', { name: '忙的、一顆' })],
    runs: { ok: run('ok'), busy: run('busy', { agent_status: 'working' }) },
    busy: {},
  })
  await openConfirm()
  assert.deepEqual(readyItems(), ['ok · 預設'])
  const busy = busyItems()
  assert.equal(busy.length, 1)
  assert.equal(busy[0], '忙的、一顆 · 預設 · 正在跑')
})

test('每列帶專案名、身份與（非 local 才有的）主機；沒有身份寫「預設」', async () => {
  useStore.setState({
    projects: [
      { id: 'p1', label: 'agents-manager', host: 'local' },
      { id: 'p2', label: 'other-proj', host: 'mac1' },
    ] as never,
    bots: [bot('a', { name: 'sn-c0a', project_id: 'p1', identity: 'cc0' }), bot('b', { name: 'remote', project_id: 'p2', identity: null }), bot('c', { name: 'busy', project_id: 'p2', identity: 'cc1' })],
    runs: { a: run('a'), b: run('b'), c: run('c', { agent_status: 'working' }) },
    busy: {},
  })
  await openConfirm()
  assert.deepEqual(readyItems(), ['sn-c0a · agents-manager · cc0', 'remote · other-proj · 預設 · mac1'])
  assert.deepEqual(busyItems(), ['busy · other-proj · cc1 · mac1 · 正在跑'])
  useStore.setState({ projects: [] as never })
})
