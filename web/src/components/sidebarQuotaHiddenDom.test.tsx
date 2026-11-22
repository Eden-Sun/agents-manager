import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { quotaDisableKey, setQuotaDisabled } from '../store/quotaHide'
import type { BotKind } from '../api/types'
import { Sidebar } from './Sidebar'

const AGY_KEY = quotaDisableKey('local', 'agy', null)
const CLAUDE_CC1_KEY = quotaDisableKey('local', 'claude', 'cc1')

afterEach(async () => {
  setQuotaDisabled(AGY_KEY, false, null)
  setQuotaDisabled(CLAUDE_CC1_KEY, false, null)
  localStorage.clear()
  resetStoreForTest()
  await unmountAll()
})
before(setupDom)
after(teardownDom)

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, kind: BotKind, identity: string | null) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind, identity, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null,
  primary: false, primary_position: 0, cwd: null,
})

beforeEach(() => {
  fakeApi()
  resetStoreForTest()
})

test('identity=null 的 agy bot 在 ag0 停用時，側欄顯示 1 個 Bot 已隱藏（ag0 已停用）', async () => {
  useStore.setState({
    projects: [project('p1')],
    bots: [bot('b1', 'agy', null)],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)

  setQuotaDisabled(AGY_KEY, true, null)
  await mount(<Sidebar />)
  await settle()

  const text = document.querySelector('.project-quota-hidden')?.textContent
  assert.equal(text, '1 個 Bot 已隱藏（ag0 已停用）')
})

test('同時有 ag0 與 cc1 停用時，顯示 2 個 Bot 已隱藏（ag0、cc1 已停用）', async () => {
  useStore.setState({
    projects: [project('p1')],
    bots: [bot('b1', 'agy', null), bot('b2', 'claude', 'cc1')],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)

  setQuotaDisabled(AGY_KEY, true, null)
  setQuotaDisabled(CLAUDE_CC1_KEY, true, null)
  await mount(<Sidebar />)
  await settle()

  const text = document.querySelector('.project-quota-hidden')?.textContent
  assert.equal(text, '2 個 Bot 已隱藏（ag0、cc1 已停用）')
})
