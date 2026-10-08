/**
 * 2026-10-08 使用者兩項：
 * - 電腦版晶片列不再 hover 開狀態卡，顏色說明改由尾端「?」開。
 * - 點選左邊 menu 的專案，側欄跟著捲到那個專案（貼齊頂端）。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'
import { UnreadChip } from './UnreadChip'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, projectId: string, primary: boolean) => ({
  id, name: `bot-${id}`, project_id: projectId, kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary, primary_position: 0, cwd: null,
})

beforeEach(() => {
  fakeApi()
  useStore.setState({
    projects: [project('p1'), project('p2')],
    bots: [bot('b1', 'p1', true), bot('b2', 'p2', false)],
    runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, hiddenBotIds: [], connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, supervisorProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

test('電腦版：滑鼠停在主力晶片上不開狀態卡；尾端「?」開顏色說明', async () => {
  await mount(<UnreadChip />)
  const chip = document.querySelector<HTMLElement>('.unread-chip[data-bot-id="b1"]')!
  await act(async () => {
    chip.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }))
    chip.dispatchEvent(new MouseEvent('mouseenter', { bubbles: false }))
    await new Promise((r) => setTimeout(r, 500))
  })
  assert.equal(document.querySelector('.bot-status-card'), null)
  assert.ok(chip.title.length > 0, '原生 tooltip 還在')
  assert.equal(document.querySelector('.chip-legend'), null)
  await click(document.querySelector<HTMLElement>('.unread-bar-help')!)
  assert.ok(document.querySelector('[role="dialog"][aria-label="主力晶片的顏色說明"]'))
})

test('點左邊 menu 的專案標題：選取並把那個專案捲到側欄頂端', async () => {
  const calls: { id: string | undefined; opts: unknown }[] = []
  const proto = Element.prototype as unknown as { scrollIntoView?: (o?: unknown) => void }
  const original = proto.scrollIntoView
  proto.scrollIntoView = function (this: HTMLElement, opts?: unknown) {
    calls.push({ id: this.closest('.project')?.getAttribute('data-project-id') ?? undefined, opts })
  }
  try {
    await mount(<Sidebar />)
    await settle()
    const label = document.querySelector<HTMLElement>('.project[data-project-id="p2"] .project-label-btn')!
    await click(label)
    await act(async () => {
      await new Promise((r) => requestAnimationFrame(() => r(null)))
    })
    assert.equal(useStore.getState().selectedProjectId, 'p2')
    const hit = calls.find((c) => c.id === 'p2' && (c.opts as { block?: string })?.block === 'start')
    assert.ok(hit, JSON.stringify(calls))
  } finally {
    if (original) proto.scrollIntoView = original
    else delete proto.scrollIntoView
  }
})
