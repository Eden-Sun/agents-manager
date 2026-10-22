/**
 * #1903：model chip 的 ⟳ 要寫出是哪一欄跟設定不同——強度 drift 曾被使用者看成 fast。
 * 窄列（精簡列、多欄落差）每欄只寫一個字，完整欄位名在 aria-label 與 tooltip 首行。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

const bot = (id: string, over: Record<string, unknown> = {}) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'codex', identity: null, model: 'gpt-6.1-sol', effort: 'medium', fast: false, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
  ...over,
})
const run = (id: string, over: Record<string, unknown> = {}) => ({
  id: `r-${id}`, bot_id: id, state: 'running', agent_status: 'idle', pane_id: `w1:${id}`, started_at: '2026-10-22T00:00:00Z',
  agent_status_since: '2026-10-22T00:00:00Z', runtime_model: 'gpt-6.1-sol', runtime_effort: 'medium', runtime_fast: false, runtime_identity: '', ...over,
})

async function render(bots: unknown[], runs: Record<string, unknown>) {
  fakeApi(() => undefined)
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj-p1', path: '/p1', host: 'local' }],
    bots, identities: [], runs, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
  await mount(<Sidebar />)
  await settle()
}
const stale = (id: string) => document.querySelector<HTMLElement>(`.bot-row[data-bot-id="${id}"] .model-tag-stale`)

test('只有強度不同：⟳ 後面寫「強度」，不是只有符號', async () => {
  await render([bot('e')], { e: run('e', { runtime_effort: 'default' }) })
  assert.equal(stale('e')?.textContent, '⟳強度')
  assert.equal(stale('e')?.getAttribute('aria-label'), '需重啟才生效：強度')
  assert.ok(document.querySelector('.model-tag.stale')!.getAttribute('title')!.startsWith('需重啟才生效：強度\n'))
})

test('只有 fast 不同：寫 fast；沒有落差：不畫 ⟳', async () => {
  await render([bot('f'), bot('ok')], { f: run('f', { runtime_fast: true }), ok: run('ok') })
  assert.equal(stale('f')?.textContent, '⟳fast')
  assert.equal(stale('ok'), null)
})

test('多欄不同：窄列每欄一個字，完整欄位在 aria-label', async () => {
  await render([bot('m')], { m: run('m', { runtime_model: 'gpt-6.1-luna', runtime_effort: 'high', runtime_fast: true }) })
  assert.equal(stale('m')?.textContent, '⟳模/強/fast')
  assert.equal(stale('m')?.getAttribute('aria-label'), '需重啟才生效：模型/強度/fast')
})
