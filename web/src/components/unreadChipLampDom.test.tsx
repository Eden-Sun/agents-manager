/**
 * 晶片列的「在跑／要你回答／等子 agent」跟燈號同一條判斷（#1000）：run 已 exited、主機斷線時不能還亮黃燈或紅燈。
 * 只看 `runs[id].agent_status` 的時候，子 agent 的 pane 死在回合中，父晶片會永遠黃色「等子 agent」。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { UnreadChip } from './UnreadChip'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  // 這裡會改全域 store（遠端主機、專案），不還原的話同一個行程裡後面的測試會讀到。
  resetStoreForTest()
  await teardownDom()
})

const project = (id: string, host = 'local') => ({ id, label: `proj-${id}`, path: `/${id}`, host })
const bot = (id: string, projectId: string, primary: boolean, parent: string | null = null) => ({
  id, name: `bot-${id}`, project_id: projectId, kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: parent, primary, primary_position: 0, cwd: null,
})
const run = (botId: string, state: string, agentStatus: string) =>
  ({ id: `run-${botId}`, bot_id: botId, state, agent_status: agentStatus, workspace_id: null, pane_id: null, started_at: new Date().toISOString() }) as never
// 主力晶片有 data-bot-id；其餘晶片沒有，就用 title 開頭的 bot 名認（`botTitle`：「bot-x。…」「bot-x 停在…」）。
const chip = (id: string) =>
  document.querySelector<HTMLElement>(`.unread-chip[data-bot-id="${id}"]`) ??
  [...document.querySelectorAll<HTMLElement>('.unread-chip')].find((e) => e.title.startsWith(`bot-${id}。`) || e.title.startsWith(`bot-${id} `)) ??
  null

beforeEach(() => {
  fakeApi()
  useStore.setState({
    projects: [project('p1'), project('p2')],
    bots: [bot('b1', 'p1', true), bot('x', 'p2', false), bot('k', 'p1', false, 'b1')],
    runs: { b1: run('b1', 'running', 'idle') },
    hosts: [],
    botOrder: {}, projectOrder: [], botUnread: {}, hiddenBotIds: [], connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, supervisorProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

test('子 agent 的 run 已 exited 不算在跑：父晶片沒有等子 agent 的黃燈；重新跑起來才出現', async () => {
  useStore.setState({ runs: { b1: run('b1', 'running', 'idle'), k: run('k', 'exited', 'working') } } as never)
  await mount(<UnreadChip />)
  assert.equal(chip('b1')!.querySelector('.unread-chip-kids'), null, 'pane 死了的子 agent 不算')
  assert.ok(!chip('b1')!.classList.contains('waits-kids'))

  await act(async () => {
    useStore.setState({ runs: { b1: run('b1', 'running', 'idle'), k: run('k', 'running', 'working') } } as never)
  })
  assert.ok(chip('b1')!.querySelector('.unread-chip-kids'), '子 agent 真的在跑，分叉圖示要出現')
  assert.ok(chip('b1')!.classList.contains('waits-kids'))
})

test('exited 的頂層 bot 不留在「在跑」那排；仍在跑時才出現並帶閃動的點', async () => {
  useStore.setState({ runs: { b1: run('b1', 'running', 'idle'), x: run('x', 'exited', 'working') } } as never)
  await mount(<UnreadChip />)
  assert.equal(chip('x'), null, '已停的 bot 不該掛在在跑那排')

  await act(async () => {
    useStore.setState({ runs: { b1: run('b1', 'running', 'idle'), x: run('x', 'running', 'working') } } as never)
  })
  // 非主力晶片要等溢出量測的 effect 跑完才穩定，直接斷言會看到中間態。
  await settle(50)
  assert.ok(chip('x'), '真的在跑就要出現')
  assert.ok(chip('x')!.querySelector('.unread-chip-dot'), '在跑的點')
})

test('主機斷線：那台 bot 最後的 blocked 不亮「要你回答」；連回來才亮（正對照）', async () => {
  const host = { name: 'm4p', ssh: 'm4p', ssh_port: 22, herdr_session: 'agents-manager', remote_path: '/r', connected: false, error: null, disconnected_since: null }
  useStore.setState({
    projects: [project('p1'), project('p3', 'm4p')],
    bots: [bot('b1', 'p1', true), bot('r', 'p3', true)],
    hosts: [host],
    runs: { b1: run('b1', 'running', 'idle'), r: run('r', 'running', 'blocked') },
  } as never)
  await mount(<UnreadChip />)
  assert.ok(chip('r'), '主力 bot 一定顯示')
  assert.ok(!chip('r')!.classList.contains('needs-reply'), '主機斷線，不算要你回答')

  await act(async () => {
    useStore.setState({ hosts: [{ ...host, connected: true }] } as never)
  })
  assert.ok(chip('r')!.classList.contains('needs-reply'), '連回來就照常亮紅燈')
})
