/**
 * 側欄父列的「子 agent」狀態點（`kidsWaitOf`）：優先序同晶片＝需要回應 → 完成待看（子 agent 已回報還沒人看）→ 跑著。
 * 一個子 agent 還在跑、另一個已經做完還沒人看：父列要亮「子 agent 已回報」，不能被「等子 agent」蓋掉。
 */
import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, parent: string | null) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: parent ? 'child' : 'user', parent_bot_id: parent,
  primary: false, primary_position: 0, cwd: null,
})
const run = (botId: string, agentStatus: string) =>
  ({ id: `run-${botId}`, bot_id: botId, state: 'running', agent_status: agentStatus, workspace_id: null, pane_id: null, started_at: new Date().toISOString() }) as never

beforeEach(() => {
  fakeApi()
  useStore.setState({
    projects: [project('p1')],
    bots: [bot('G', null), bot('K1', 'G'), bot('K2', 'G')],
    runs: { G: run('G', 'idle'), K1: run('K1', 'idle'), K2: run('K2', 'working') },
    botOrder: {}, projectOrder: [], botUnread: { K1: 1 }, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'G', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

test('子 agent 已回報還沒人看，同時另一個還在跑：父列亮「已回報」（reply），不是「等子 agent」（busy）', async () => {
  await mount(<Sidebar />)
  await settle()
  const wait = document.querySelector<HTMLElement>('[data-bot-id="G"] .bot-kids-wait')
  assert.ok(wait, '父列要有子 agent 的狀態點')
  assert.ok(wait.classList.contains('reply'), '完成待看優先於跑著')
})
