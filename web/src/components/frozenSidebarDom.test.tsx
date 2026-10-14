/**
 * 冰凍（「不用保溫」，SPEC §6.5k）在側欄：按了的主力列名旁畫雪花；沒按的主力、非主力都不畫。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

const bot = (id: string, primary: boolean) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary, primary_position: 0, cwd: null,
})
const run = (id: string, skip: boolean) => ({ id: `r-${id}`, bot_id: id, state: 'running', agent_status: 'idle', keep_warm_skip: skip })

test('側欄：按了「不用保溫」的主力列名旁畫雪花，沒按的主力與非主力不畫', async () => {
  fakeApi(() => undefined)
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj-p1', path: '/p1', host: 'local' }],
    bots: [bot('frozen', true), bot('warm', true), bot('plain', false)],
    identities: [],
    runs: { frozen: run('frozen', true), warm: run('warm', false), plain: run('plain', true) },
    botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
  await mount(<Sidebar />)
  await settle()
  const badge = (id: string) => document.querySelector<HTMLElement>(`.bot-row[data-bot-id="${id}"] .frozen-badge`)
  assert.ok(badge('frozen'), '主力、按了不用保溫 → 雪花')
  assert.equal(badge('frozen')!.querySelector('svg') !== null, true, '用 inline SVG')
  assert.equal(badge('warm'), null, '主力沒按 → 不畫')
  assert.equal(badge('plain'), null, '非主力就算 run 上有旗標也不畫')
})
