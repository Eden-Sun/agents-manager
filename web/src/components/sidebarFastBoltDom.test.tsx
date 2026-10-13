/**
 * #1682：側欄列（主列與子 agent 精簡列）上，codex 正在跑 fast 要一眼看得出來——model 標籤窄列會被截斷，
 * fast 只在 tooltip 等於看不見。照 UI-DECISIONS「codex fast 在 logo 上疊閃電」：閃電疊在 kind logo 上（不會被截），
 * 關著的不畫（不佔位）；來源是正在跑的那一份（`status.fast_mode` → `runtime_fast` → 設定值），不是只看設定。
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
  id, name: `bot-${id}`, project_id: 'p1', kind: 'codex', identity: null, model: 'gpt-6.1-sol', effort: 'low', fast: false, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
  ...over,
})
const run = (id: string, over: Record<string, unknown> = {}) => ({
  id: `r-${id}`, bot_id: id, state: 'running', agent_status: 'idle', pane_id: `w1:${id}`, started_at: '2026-10-13T00:00:00Z',
  agent_status_since: '2026-10-13T00:00:00Z', runtime_model: null, runtime_effort: null, runtime_fast: null, ...over,
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
const row = (id: string) => document.querySelector<HTMLElement>(`.bot-row[data-bot-id="${id}"]`)!

test('主列：codex 回報 fast 開著 → logo 上有閃電；關著的列不畫', async () => {
  await render(
    [bot('on'), bot('off')],
    { on: run('on', { status: { fast_mode: true } }), off: run('off', { status: { fast_mode: false } }) },
  )
  assert.ok(row('on').querySelector('.kind-tag.fast .kind-fast'), 'fast 開著要在 kind logo 上疊閃電')
  assert.match(row('on').querySelector('.kind-tag')!.getAttribute('aria-label') ?? '', /fast/)
  assert.equal(row('off').querySelector('.kind-fast'), null, 'fast 關著不畫、不佔位')
})

test('子 agent 精簡列：fast 開著 → logo 上有閃電（模型標籤被截斷時也看得到）', async () => {
  await render(
    [bot('lead', { kind: 'claude', model: 'opus', effort: 'medium' }), bot('kid', { parent_bot_id: 'lead' })],
    { lead: run('lead'), kid: run('kid', { status: { fast_mode: true } }) },
  )
  const kid = row('kid')
  assert.ok(kid.classList.contains('compact'), '子 agent 列是精簡列')
  assert.ok(kid.querySelector('.bot-kind-wrap .kind-tag.fast .kind-fast'), '精簡列的 logo 要有閃電')
  assert.equal(row('lead').querySelector('.kind-fast'), null, '母 bot（claude）沒有 fast')
})

test('沒有 CLI 回報時看 runtime_fast（argv 收編來的），再退回設定值', async () => {
  await render(
    [bot('rt'), bot('cfg', { fast: true }), bot('stale', { fast: true })],
    {
      rt: run('rt', { runtime_fast: 1 }),
      // 設定寫 fast、但實際在跑的沒有：看實際的，不看設定（設定改了還沒重啟時兩者不同）。
      stale: run('stale', { runtime_fast: 0 }),
    },
  )
  assert.ok(row('rt').querySelector('.kind-fast'), 'runtime_fast 為真要畫')
  assert.ok(row('cfg').querySelector('.kind-fast'), '沒有 run 時退回設定值')
  assert.equal(row('stale').querySelector('.kind-fast'), null, '實際在跑的不是 fast，不畫')
})
