/**
 * 側欄的鍵盤／滑鼠互動（真的掛 `Sidebar` 進 happy-dom，發事件、看 store 與 DOM）：
 * - #694 bot 列只處理「列本身」發出的 Enter／Space；列裡的 ⋯ 按鈕、選單項、確認框的按鍵與點擊不被攔、不選取這顆 bot。
 * - project ⋯ 選單與它的確認框不會連帶選取專案。
 */
import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, keydown, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
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

test('額度晶片的提示點名實際在跑的身分', async () => {
  const critical = { used_pct: 98, low: true, critical: true, resets_at: null }
  useStore.setState({
    bots: [{ ...bot('b1', 'p1'), identity: 'cc1' }],
    runs: {
      b1: {
        id: 'r1',
        bot_id: 'b1',
        state: 'running',
        agent_status: 'idle',
        runtime_identity: 'cc2',
        runtime_model: null,
        runtime_effort: null,
        runtime_fast: null,
      },
    },
    identities: [
      { name: 'cc1', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '/a' }, args: [], host: null },
      { name: 'cc2', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '/b' }, args: [], host: null },
    ],
    quota: { 'claude:cc2': { five_hour: critical, seven_day: null, fable: null } },
  } as never)

  await mount(<Sidebar />)
  await settle()

  const chip = row('b1').querySelector<HTMLElement>('.bot-quota-chip.crit')!
  assert.ok(chip, '應顯示 critical 晶片')
  assert.ok(chip.title.includes('cc2'), `title 應含 cc2: ${chip.title}`)
  assert.ok(!chip.title.includes('cc1'), `title 不應含 cc1: ${chip.title}`)

  // 對照：把 run 拿掉（runs: {}）、quota 改放 claude:cc1 → title 含 cc1
  await act(async () => {
    useStore.setState({
      runs: {},
      quota: { 'claude:cc1': { five_hour: critical, seven_day: null, fable: null } },
    } as never)
  })
  await settle()
  const chipWithoutRun = row('b1').querySelector<HTMLElement>('.bot-quota-chip.crit')!
  assert.ok(chipWithoutRun, '無 run 時仍應顯示 critical 晶片')
  assert.ok(chipWithoutRun.title.includes('cc1'), `title 應含 cc1: ${chipWithoutRun.title}`)
})

/** 側欄底部「新增 Bot」開哪個專案的表單（#1085）：選著 bot 就用那顆 bot 所在的專案，不是清單第一個。 */
const addBotButton = () => [...document.querySelectorAll<HTMLButtonElement>('.sidebar-foot-actions button')].find((b) => b.textContent?.trim() === '新增 Bot')!
const sheetProject = () => document.querySelector('.modal .modal-sub')?.textContent

test('底部「新增 Bot」：選著 p2 的 bot 就開 p2 的表單', async () => {
  await act(async () => useStore.setState({ selectedBotId: 'b3', selectedProjectId: null } as never))
  await mount(<Sidebar />)
  await settle()
  await click(addBotButton())
  await settle()
  assert.equal(sheetProject(), 'proj-p2')
})

test('選著 p1 的 bot 開 p1', async () => {
  await act(async () => useStore.setState({ selectedBotId: 'b2', selectedProjectId: null } as never))
  await mount(<Sidebar />)
  await settle()
  await click(addBotButton())
  await settle()
  assert.equal(sheetProject(), 'proj-p1')
})

test('選著群組頁以群組為準', async () => {
  await act(async () => useStore.setState({ selectedBotId: 'b2', selectedProjectId: 'p2' } as never))
  await mount(<Sidebar />)
  await settle()
  await click(addBotButton())
  await settle()
  assert.equal(sheetProject(), 'proj-p2')
})

test('什麼都沒選退回第一個專案', async () => {
  await act(async () => useStore.setState({ selectedBotId: null, selectedProjectId: null } as never))
  await mount(<Sidebar />)
  await settle()
  await click(addBotButton())
  await settle()
  assert.equal(sheetProject(), 'proj-p1')
})

test('子 agent 收合時 ↓ 跳過它們，走到下一個父列；↑ 走回來', async () => {
  localStorage.setItem('am.collapsedChildren', JSON.stringify(['b1']))
  try {
    useStore.setState({
      bots: [
        bot('b1', 'p1'),
        { ...bot('c1', 'p1'), parent_bot_id: 'b1', managed_by: 'child' },
        bot('b2', 'p1'),
      ],
      selectedBotId: 'b1',
    } as never)
    await mount(<Sidebar />)
    await settle()
    assert.equal(document.querySelector('[data-bot-id="c1"]'), null, 'c1 應被收合不畫在 DOM')
    row('b1').focus()
    await keydown(row('b1'), 'ArrowDown')
    await settle()
    assert.equal(selected(), 'b2', '↓ 應跳過收合的子 agent 選取 b2')
    assert.equal(document.activeElement, row('b2'), '焦點應移到 b2')
    await keydown(row('b2'), 'ArrowUp')
    await settle()
    assert.equal(selected(), 'b1', '↑ 應跳過收合的子 agent 選回 b1')
    assert.equal(document.activeElement, row('b1'), '焦點應移回 b1')
  } finally {
    localStorage.removeItem('am.collapsedChildren')
  }
})

test('展開時照舊走進子 agent（防回歸）', async () => {
  useStore.setState({
    bots: [
      bot('b1', 'p1'),
      { ...bot('c1', 'p1'), parent_bot_id: 'b1', managed_by: 'child' },
      bot('b2', 'p1'),
    ],
    selectedBotId: 'b1',
  } as never)
  await mount(<Sidebar />)
  await settle()
  assert.ok(row('c1'), '未收合時 c1 在 DOM')
  row('b1').focus()
  await keydown(row('b1'), 'ArrowDown')
  await settle()
  assert.equal(selected(), 'c1', '展開時應走進子 agent c1')
})

test('搜尋中 ↓ 只走畫出來的列', async () => {
  useStore.setState({
    bots: [
      bot('b1', 'p1'),
      bot('b2', 'p1'),
      bot('b3', 'p2'),
    ],
    selectedBotId: 'b1',
  } as never)
  await mount(<Sidebar />)
  await settle()
  const searchInput = document.querySelector<HTMLInputElement>('.bot-search-input')!
  assert.ok(searchInput)
  await typeInto(searchInput, 'bot-b1')
  await settle()
  row('b1').focus()
  await keydown(row('b1'), 'ArrowDown')
  await settle()
  assert.notEqual(selected(), 'b2', '搜尋濾掉 b2 時不能跳到 b2')
  assert.equal(selected(), 'b1', '只有一列可走時維持原選取')
})
