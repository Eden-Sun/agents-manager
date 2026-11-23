import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, click, mockApi, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'
import { BotRowMenu } from './BotRowMenu'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mk = (role: string | null) =>
  ({ id: 'b1', name: 'wrk-1', project_id: 'p1', kind: 'claude', role, model: 'opus', effort: 'medium', fast: false, identity: null, persona: null, live_apply_deferred: false, needs_restart: false, managed_by: 'user', manager_use: true }) as unknown as Bot

function setupBackend(schedules: Array<{ id: string; bot_id: string }>) {
  return mockApi({
    async request(method, path) {
      if (method === 'GET' && path === '/projects/p1/schedules') {
        return { schedules: schedules.map((s) => ({ ...s, project_id: 'p1', name: 'nightly', open: null })) }
      }
      if (method === 'DELETE' && path === '/bots/b1') return null
      return {}
    },
    async upload() {
      return {}
    },
  })
}

const deleteRequests = (reqs: { method: string; path: string }[]) =>
  reqs.filter((r) => r.method === 'DELETE' && r.path.endsWith('/bots/b1'))

const settingsDeleteButton = () =>
  [...document.querySelectorAll<HTMLButtonElement>('.bs-danger button')].find((b) => b.textContent?.trim() === '刪除 Bot')

function ExpertHarness({ bot }: { bot: Bot }) {
  const open = useStore((s) => s.settingsBotId === 'b1')
  return open ? <BotSettingsPanel botId={bot.id} /> : null
}

async function openSettings(bot: Bot) {
  useStore.setState({ bots: [bot], runs: {}, busy: {}, selectedBotId: 'b1', settingsBotId: 'b1', refreshState: async () => {} })
  await mount(<ExpertHarness bot={bot} />)
}

test('執行者：設定頁的刪除直接刪，不開確認框', async () => {
  const reqs = setupBackend([])
  await openSettings(mk('executor'))
  await click(settingsDeleteButton()!)
  await until(() => deleteRequests(reqs).length === 1, 'delete request')
  assert.equal(document.querySelector('[role="alertdialog"]'), null)
})

test('執行者綁著排程：設定頁仍開確認框，不直接刪', async () => {
  const reqs = setupBackend([{ id: 's1', bot_id: 'b1' }])
  await openSettings(mk('executor'))
  await click(settingsDeleteButton()!)
  await until(() => document.querySelector('[role="alertdialog"]') !== null, 'confirm dialog')
  assert.equal(deleteRequests(reqs).length, 0)
})

test('非執行者（顧問）：設定頁仍開確認框', async () => {
  const reqs = setupBackend([])
  await openSettings(mk('advisor'))
  await click(settingsDeleteButton()!)
  await until(() => document.querySelector('[role="alertdialog"]') !== null, 'confirm dialog')
  assert.equal(deleteRequests(reqs).length, 0)
})

test('執行者：列表選單的刪除直接刪，不開確認框', async () => {
  const reqs = setupBackend([])
  useStore.setState({ bots: [mk('executor')], runs: {}, busy: {}, refreshState: async () => {} })
  await mount(<BotRowMenu botId="b1" />)
  await click(document.querySelector<HTMLButtonElement>('button[aria-label="wrk-1 的操作"]')!)
  const item = [...document.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find((b) => b.textContent?.trim() === '刪除…')!
  await click(item)
  await until(() => deleteRequests(reqs).length === 1, 'delete request')
  assert.equal(document.querySelector('[role="alertdialog"]'), null)
})

test('非執行者（管理者以外的一般 bot）：列表選單的刪除仍開確認框', async () => {
  const reqs = setupBackend([])
  useStore.setState({ bots: [mk('advisor')], runs: {}, busy: {}, refreshState: async () => {} })
  await mount(<BotRowMenu botId="b1" />)
  await click(document.querySelector<HTMLButtonElement>('button[aria-label="wrk-1 的操作"]')!)
  const item = [...document.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')].find((b) => b.textContent?.trim() === '刪除…')!
  await click(item)
  await until(() => document.querySelector('[role="alertdialog"]') !== null, 'confirm dialog')
  assert.equal(deleteRequests(reqs).length, 0)
})
