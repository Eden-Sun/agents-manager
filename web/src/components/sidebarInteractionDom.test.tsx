/**
 * 側欄的鍵盤／滑鼠互動（真的掛 `Sidebar` 進 happy-dom，發事件、看 store 與 DOM）：
 * - #694 bot 列只處理「列本身」發出的 Enter／Space；列裡的 ⋯ 按鈕、選單項、確認框的按鍵與點擊不被攔、不選取這顆 bot。
 * - project ⋯ 選單與它的確認框不會連帶選取專案。
 */
import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, projectId: string) => ({
  id, name: `bot-${id}`, project_id: projectId, kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
})

beforeEach(() => {
  fakeApi()
  useStore.setState({
    projects: [project('p1'), project('p2')],
    bots: [bot('b1', 'p1'), bot('b2', 'p1'), bot('b3', 'p2')],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'b2', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

const row = (id: string) => document.querySelector<HTMLElement>(`[data-bot-id="${id}"]`)!
const rowMenuButton = (id: string) => row(id).querySelector<HTMLButtonElement>('.bot-actions .head-menu-btn')!
const selected = () => useStore.getState().selectedBotId

test('#694 列本身按 Enter／Space：選取這顆 bot（並擋掉預設）', async () => {
  await mount(<Sidebar />)
  await settle()
  const enter = await keydown(row('b1'), 'Enter')
  assert.equal(selected(), 'b1')
  assert.equal(enter.defaultPrevented, true)
  await act(async () => useStore.setState({ selectedBotId: 'b2' } as never))
  const space = await keydown(row('b1'), ' ')
  assert.equal(selected(), 'b1')
  assert.equal(space.defaultPrevented, true)
})

test('#694 列裡的 ⋯ 按鈕按 Enter／Space：列不攔（預設動作才會啟動按鈕）、也不選取這顆 bot', async () => {
  await mount(<Sidebar />)
  await settle()
  const button = rowMenuButton('b1')
  for (const key of ['Enter', ' ']) {
    const e = await keydown(button, key)
    assert.equal(e.defaultPrevented, false, `${JSON.stringify(key)} 的預設動作不能被列取消`)
    assert.equal(selected(), 'b2', '不能因此選取 b1')
  }
})

test('#694 點列裡的 ⋯ 展開選單：不選取這顆 bot；選單項與確認框的點擊也不會', async () => {
  await mount(<Sidebar />)
  await settle()
  await click(rowMenuButton('b1'))
  assert.equal(selected(), 'b2')
  const menu = row('b1').querySelector('[role=menu]')
  assert.ok(menu, '選單要展開')
  const deleteItem = [...menu!.querySelectorAll('button')].find((b) => b.textContent?.includes('刪除'))
  assert.ok(deleteItem, '選單裡要有刪除')
  await click(deleteItem!)
  assert.equal(selected(), 'b2')
  const dialog = document.querySelector('[role=alertdialog]')
  assert.ok(dialog, '刪除要先跳確認框')
  await click(dialog!.querySelector('.confirm-title')!)
  assert.equal(selected(), 'b2', '點確認框的標題不能冒泡成選取這顆 bot')
  const cancel = [...dialog!.querySelectorAll('button')].find((b) => b.textContent === '取消')!
  await click(cancel)
  assert.equal(selected(), 'b2', '按取消也不行')
  assert.equal(document.querySelector('[role=alertdialog]'), null, '取消要關掉確認框')
})

test('project 標題列的 ⋯ 與它的選單項、確認框：不選取專案', async () => {
  await mount(<Sidebar />)
  await settle()
  const header = document.querySelector<HTMLElement>('.project-head')!
  const more = header.querySelector<HTMLButtonElement>('.head-menu-btn')!
  await click(more)
  assert.equal(useStore.getState().selectedProjectId, null, '點 ⋯ 不能選取專案')
  const menu = header.querySelector('[role=menu]')
  assert.ok(menu, '選單要展開')
  const del = [...menu!.querySelectorAll('button')].find((b) => b.textContent?.includes('刪除專案'))!
  await click(del)
  assert.equal(useStore.getState().selectedProjectId, null)
  const dialog = document.querySelector('[role=alertdialog]')
  assert.ok(dialog, '刪除專案要先跳確認框')
  await click(dialog!.querySelector('.confirm-title')!)
  assert.equal(useStore.getState().selectedProjectId, null, '點確認框不能冒泡成選取專案')
})

test('額度 critical 保留模型標籤，一般列與精簡子列都顯示短紅晶片', async () => {
  const critical = { used_pct: 98, low: true, critical: true, resets_at: null }
  const parent = { ...bot('b1', 'p1'), model: 'claude-opus-5' }
  const child = { ...bot('child', 'p1'), model: 'claude-opus-5', parent_bot_id: 'b1' }
  useStore.setState({
    bots: [parent, child],
    quota: { claude: { five_hour: critical, seven_day: null, fable: null } },
  } as never)

  await mount(<Sidebar />)
  await settle()

  for (const id of ['b1', 'child']) {
    const botRow = row(id)
    const model = botRow.querySelector<HTMLElement>('.model-tag')!
    const warning = botRow.querySelector<HTMLElement>('.bot-quota-chip.crit')!
    assert.equal(model.textContent, 'opus', `${id} 的模型仍完整顯示`)
    assert.match(model.title, /claude-opus-5/, `${id} 保留模型 tooltip`)
    assert.equal(warning.textContent?.trim(), '⚠ 2%', `${id} 用精簡警告`)
    assert.match(warning.title, /5h 額度剩 2%，快用完了/, `${id} 的 tooltip 保留完整說明`)
  }
  assert.equal(row('child').classList.contains('compact'), true, '子列仍走精簡版型')
  assert.ok(row('b1').querySelector('.lamp'), '狀態燈照常保留')
})
