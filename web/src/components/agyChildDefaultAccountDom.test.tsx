/**
 * 子 agent 跑在 kind 的預設帳號上（agy 的真家目錄、沒有空 env 身份）：側欄身份晶片要顯示「預設」，不是「未知」。
 * 「未知」只代表 run 還沒記到身份（runtime_identity＝NULL）；預設帳號是 daemon 查過之後記下的空字串。
 */
import test, { afterEach, before, beforeEach, after } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = { id: 'p1', label: 'proj-p1', path: '/p1', host: 'm4p' }
const child = {
  id: 'child', name: 'agycleanup', project_id: 'p1', kind: 'agy', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'child', parent_bot_id: 'parent', primary: false, primary_position: 0, cwd: null,
}
const parent = { ...child, id: 'parent', name: 'agy-parent', managed_by: 'user', parent_bot_id: null }

beforeEach(() => {
  fakeApi()
})

const run = (runtime_identity: string | null) => ({
  id: 'r1', bot_id: 'child', state: 'running', agent_status: 'idle', runtime_identity, runtime_model: null, runtime_effort: null, runtime_fast: null,
})

async function badgeFor(runtime_identity: string | null): Promise<HTMLElement | null> {
  useStore.setState({
    projects: [project], bots: [parent, child], runs: { child: run(runtime_identity) }, botOrder: {}, projectOrder: [], botUnread: {},
    connected: true, defaultConnected: true, socket: 'open', selectedBotId: 'parent', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
    identities: [{ name: 'ag1', kind: 'agy', env: { HOME: '$HOME/.agy-' }, args: [], host: 'm4p' }],
  } as never)
  await mount(<Sidebar />)
  await settle()
  return document.querySelector<HTMLElement>('[data-bot-id="child"] .kind-identity .identity-badge')
}

test('agy 子 agent 在預設帳號上（runtime_identity 空字串）：晶片顯示「預設」，不是「未知」', async () => {
  const badge = await badgeFor('')
  assert.ok(badge, '應該畫出身份晶片')
  assert.equal(badge.textContent?.trim(), '預設')
  assert.ok(!badge.classList.contains('is-unknown'))
})

test('runtime_identity 真的沒記（NULL）時仍顯示「未知」', async () => {
  const badge = await badgeFor(null)
  assert.ok(badge)
  assert.equal(badge.textContent?.trim(), '未知')
  assert.ok(badge.classList.contains('is-unknown'))
})
