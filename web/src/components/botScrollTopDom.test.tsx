import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { requestBotToTop } from '../lib/scrollBot'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'
import { UnreadChip } from './UnreadChip'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, projectId: string, parent: string | null, primary: boolean) => ({
  id, name: `bot-${id}`, project_id: projectId, kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: parent ? 'child' : 'user', parent_bot_id: parent, primary, primary_position: 0, cwd: null,
})
const frame = () => new Promise((r) => requestAnimationFrame(() => r(null)))

beforeEach(() => {
  localStorage.clear()
  fakeApi()
  useStore.setState({
    projects: [project('p1'), project('p2')],
    bots: [bot('a', 'p1', null, false), bot('G', 'p2', null, false), bot('P', 'p2', 'G', false), bot('C', 'p2', 'P', true)],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, hiddenBotIds: [], connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'a', selectedProjectId: null, supervisorProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

function stubLayout() {
  const calls: { top: number | undefined; behavior: unknown }[] = []
  const scroller = document.querySelector<HTMLElement>('.sidebar-scroll')!
  Object.defineProperty(scroller, 'scrollTop', { value: 50, writable: true, configurable: true })
  scroller.getBoundingClientRect = () => ({ top: 100, bottom: 600, left: 0, right: 300, width: 300, height: 500, x: 0, y: 100, toJSON: () => ({}) })
  ;(scroller as unknown as { scrollTo: (o: { top?: number; behavior?: unknown }) => void }).scrollTo = (o) => calls.push({ top: o.top, behavior: o.behavior })
  const rect = Element.prototype.getBoundingClientRect
  Element.prototype.getBoundingClientRect = function (this: Element) {
    if (this.matches('.bot-row[data-bot-id]')) {
      return { top: 460, bottom: 500, left: 0, right: 300, width: 300, height: 40, x: 0, y: 460, toJSON: () => ({}) } as DOMRect
    }
    return rect.call(this)
  }
  return { calls, restore: () => { Element.prototype.getBoundingClientRect = rect } }
}

test('點主力晶片：專案與祖先子列收合時先展開，再把那一列捲到側欄頂端', async () => {
  localStorage.setItem('am.collapsedProjects', JSON.stringify(['p2']))
  localStorage.setItem('am.collapsedChildren', JSON.stringify(['G', 'P']))
  await mount(
    <>
      <UnreadChip />
      <Sidebar />
    </>,
  )
  await settle()
  assert.equal(document.querySelector('.bot-row[data-bot-id="C"]'), null, '收合時孫代列不在 DOM')
  const { calls, restore } = stubLayout()
  try {
    await click(document.querySelector<HTMLElement>('.unread-chip[data-bot-id="C"]')!)
    await act(async () => {
      await frame()
      await frame()
    })
    assert.equal(useStore.getState().selectedBotId, 'C')
    assert.ok(document.querySelector('.bot-row[data-bot-id="C"]'), '祖先與專案都要展開，孫代列才畫得出來')
    assert.deepEqual(calls.at(-1), { top: 410, behavior: 'smooth' }, JSON.stringify(calls))
  } finally {
    restore()
  }
})

test('已經選著的主力 bot 再點一次晶片也要捲到頂端（選取沒變不能讓捲動消失）', async () => {
  useStore.setState({ selectedBotId: 'C' } as never)
  await mount(
    <>
      <UnreadChip />
      <Sidebar />
    </>,
  )
  await settle()
  const { calls, restore } = stubLayout()
  try {
    await click(document.querySelector<HTMLElement>('.unread-chip[data-bot-id="C"]')!)
    await act(async () => {
      await frame()
      await frame()
    })
    assert.deepEqual(calls.at(-1), { top: 410, behavior: 'smooth' }, JSON.stringify(calls))
  } finally {
    restore()
  }
})

test('主力晶片的置頂請求先被處理掉，選取換了的 nearest 捲動也不能打斷置頂（#1786）', async () => {
  const intoView: string[] = []
  const origIntoView = Element.prototype.scrollIntoView
  Element.prototype.scrollIntoView = function (this: Element) {
    intoView.push(this.getAttribute('data-bot-id') ?? '')
  }
  try {
    await mount(<Sidebar />)
    await settle()
    await act(async () => {
      await frame()
      await frame()
    })
    intoView.length = 0
    const { calls, restore } = stubLayout()
    try {
      await act(async () => {
        useStore.getState().selectBot('C')
        requestBotToTop('C')
      })
      await act(async () => {
        await frame()
        await frame()
      })
      assert.deepEqual(intoView, [], '選取換了的 nearest 不該碰 C 那列')
      assert.deepEqual(calls.at(-1), { top: 410, behavior: 'smooth' }, JSON.stringify(calls))
    } finally {
      restore()
    }
  } finally {
    Element.prototype.scrollIntoView = origIntoView
  }
})

test('非主力入口（只靠選取換了）仍是原本的 nearest，不置頂', async () => {
  await mount(<Sidebar />)
  await settle()
  const { calls, restore } = stubLayout()
  try {
    await act(async () => {
      useStore.getState().selectBot('G')
      await frame()
      await frame()
    })
    assert.equal(calls.length, 0, '一般選取不該呼叫置頂捲動')
  } finally {
    restore()
  }
})
