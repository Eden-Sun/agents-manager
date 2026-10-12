/**
 * 晶片的顏色優先序＝緊急度（`UnreadChip` 檔頭、`BotStatusCard` 的 headline）：需要回應 → 完成待看（未讀）→ 等子 agent → 在跑。
 * 父 bot 的子 agent 還在跑、同時父 bot 有一則做完還沒看的回合：要亮的是「完成待看」，不能被「等子 agent」的黃點蓋掉。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { UnreadChip } from './UnreadChip'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, parent: string | null = null) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: parent, primary: false, primary_position: 0, cwd: null,
})
const run = (botId: string, state: string, agentStatus: string) =>
  ({ id: `run-${botId}`, bot_id: botId, state, agent_status: agentStatus, workspace_id: null, pane_id: null, started_at: new Date().toISOString() }) as never
const chip = (id: string) =>
  [...document.querySelectorAll<HTMLElement>('.unread-chip')].find((e) => e.title.startsWith(`bot-${id}。`) || e.title.startsWith(`bot-${id} `)) ?? null

beforeEach(() => {
  fakeApi()
  useStore.setState({
    projects: [project('p1')],
    bots: [bot('b1'), bot('k', 'b1')],
    runs: { b1: run('b1', 'running', 'idle'), k: run('k', 'running', 'working') },
    hosts: [],
    botOrder: {}, projectOrder: [], botUnread: { b1: 2 }, hiddenBotIds: [], connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, supervisorProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

test('完成待看（未讀）蓋過等子 agent：晶片的狀態 class 是 unread，不是 waits-kids', async () => {
  await mount(<UnreadChip />)
  await settle()
  const c = chip('b1')
  assert.ok(c, '父晶片要在')
  assert.ok(c.classList.contains('unread'), '有做完還沒看的回合，要亮完成待看')
  assert.ok(!c.classList.contains('waits-kids'), '不能被子 agent 的黃點蓋掉')
})
