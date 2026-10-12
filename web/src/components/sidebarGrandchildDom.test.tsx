/**
 * 孫代（子 agent 底下再掛的子 agent，SPEC §6.5a：「孫代因此掛在子代下面」）：側欄的 bot 樹一定要畫得出來。
 * 以前 `Sidebar` 只畫兩層（頂層與它的直屬子列），孫代在 store 裡看得到、畫面上卻找不到一列。
 */
import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { orderedBotIds, useStore } from '../store/store'
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

beforeEach(() => {
  fakeApi()
  useStore.setState({
    projects: [project('p1')],
    bots: [bot('G', null), bot('P', 'G'), bot('C', 'P')],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'G', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

test('孫代（子的子）有自己的列，收在頂層父列的子列裡，並且縮排一格（deep）', async () => {
  await mount(<Sidebar />)
  await settle()
  const grandchild = document.querySelector<HTMLElement>('[data-bot-id="C"]')
  assert.ok(grandchild, '孫代要畫得出來，不能只在 store 裡')
  const kids = document.querySelector<HTMLElement>('#bot-kids-G')
  assert.ok(kids?.contains(grandchild), '孫代要收在頂層父列（G）的子列裡')
  assert.ok(grandchild.closest('.bot-child')?.classList.contains('deep'), '孫代要比直屬子列縮排一格')
  assert.ok(document.querySelector('[data-bot-id="P"]'), '子代還是照樣畫')
})

test('鍵盤 ↑／↓ 的順序照畫面：孫代緊跟在它的子代後面（深度優先）', () => {
  const st = {
    projects: [project('p1')],
    projectOrder: [],
    bots: [bot('G', null), bot('P', 'G'), bot('C', 'P'), bot('D', 'G')],
    botOrder: {},
    hiddenBotIds: [],
  }
  assert.deepEqual(orderedBotIds(st as never), ['G', 'P', 'C', 'D'])
})

test('孤兒子 agent（父列不在清單裡）也要畫得出來：照頂層列處理，不能從側欄消失', async () => {
  useStore.setState({ bots: [bot('C', 'GONE'), bot('X', null)], runs: {}, selectedBotId: 'X' } as never)
  await mount(<Sidebar />)
  await settle()
  assert.ok(document.querySelector('[data-bot-id="C"]'), '父列不見了，子列也要留在側欄上（鍵盤順序已經把它算進來）')
  assert.ok(document.querySelector('[data-bot-id="X"]'))
})
