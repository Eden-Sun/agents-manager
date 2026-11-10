import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'
import type { PatchBotInput } from '../api/types'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

let patchedBots: { id: string; input: PatchBotInput }[] = []
const realPatchBot = useStore.getState().patchBot

beforeEach(() => {
  fakeApi()
  patchedBots = []
  const mockPatchBot = async (id: string, input: PatchBotInput) => {
    patchedBots.push({ id, input })
    useStore.setState((s) => ({
      bots: s.bots.map((b) => (b.id === id ? { ...b, ...input } : b)),
    }))
    return { needsRestart: false, deferred: false, remappedModel: null }
  }

  useStore.setState({
    patchBot: mockPatchBot,
    projects: [
      {
        id: 'p-expert',
        label: '專家專案',
        path: '/expert',
        host: 'local',
        mode: 'expert',
      },
      {
        id: 'p-plain',
        label: '一般專案',
        path: '/plain',
        host: 'local',
        mode: 'parent_child',
      },
    ],
    bots: [
      {
        id: 'b-mgr',
        name: 'mgr',
        project_id: 'p-expert',
        kind: 'claude',
        role: 'manager',
        manager_use: true,
        identity: null,
        model: null,
        effort: null,
        args: [],
        env: {},
        autostart: false,
        inject_hooks: true,
        auto_approve: false,
        managed_by: 'user',
        parent_bot_id: null,
        primary: false,
        primary_position: 0,
        cwd: null,
      },
      {
        id: 'b-exec',
        name: 'exec-1',
        project_id: 'p-expert',
        kind: 'claude',
        role: 'executor',
        manager_use: true,
        identity: null,
        model: null,
        effort: null,
        args: [],
        env: {},
        autostart: false,
        inject_hooks: true,
        auto_approve: false,
        managed_by: 'user',
        parent_bot_id: null,
        primary: false,
        primary_position: 0,
        cwd: null,
      },
      {
        id: 'b-adv',
        name: 'adv-1',
        project_id: 'p-expert',
        kind: 'codex',
        role: 'advisor',
        manager_use: false,
        identity: null,
        model: null,
        effort: null,
        args: [],
        env: {},
        autostart: false,
        inject_hooks: true,
        auto_approve: false,
        managed_by: 'user',
        parent_bot_id: null,
        primary: false,
        primary_position: 0,
        cwd: null,
      },
      {
        id: 'b-spec',
        name: 'spec-1',
        project_id: 'p-expert',
        kind: 'claude',
        role: 'specialist',
        track: 'infra',
        manager_use: true,
        identity: null,
        model: null,
        effort: null,
        args: [],
        env: {},
        autostart: false,
        inject_hooks: true,
        auto_approve: false,
        managed_by: 'user',
        parent_bot_id: null,
        primary: false,
        primary_position: 0,
        cwd: null,
      },
      {
        id: 'b-plain',
        name: 'plain-1',
        project_id: 'p-plain',
        kind: 'claude',
        role: 'executor',
        manager_use: true,
        identity: null,
        model: null,
        effort: null,
        args: [],
        env: {},
        autostart: false,
        inject_hooks: true,
        auto_approve: false,
        managed_by: 'user',
        parent_bot_id: null,
        primary: false,
        primary_position: 0,
        cwd: null,
      },
    ],
    runs: {},
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    connected: true,
    defaultConnected: true,
    socket: 'open',
    selectedBotId: 'b-mgr',
    selectedProjectId: null,
    sidePanes: {},
    unownedPanes: [],
    busy: {},
  } as never)
})

afterEach(() => {
  useStore.setState({ patchBot: realPatchBot })
})

const menuItems = () => [...document.querySelectorAll<HTMLButtonElement>('.head-menu-item')]

test('專家模式側欄：manager_use=false 整列淡化，並有停派標記（不是按鈕、不蓋圖示）', async () => {
  await mount(<Sidebar />)
  await settle()

  const advRow = document.querySelector<HTMLElement>('[data-bot-id="b-adv"]')!
  assert.ok(advRow.classList.contains('manager-disabled'))
  const tag = advRow.querySelector('.bot-manager-disabled-tag')!
  assert.equal(tag.tagName, 'SPAN')
  assert.equal(tag.querySelector('.bot-nodispatch-label')?.textContent, '停派')
  assert.ok(tag.querySelector('svg'), '停派標記要有禁用 icon')
  assert.match(tag.getAttribute('title') ?? '', /管理者不能派工/)
  assert.equal(advRow.querySelector('.bot-manager-use-checkbox'), null)
  const kind = advRow.querySelector('.bot-kind-wrap')!
  assert.equal((tag.compareDocumentPosition(kind) & Node.DOCUMENT_POSITION_PRECEDING) !== 0, true, '標記在 kind 圖示後面，不蓋住它')

  const execRow = document.querySelector<HTMLElement>('[data-bot-id="b-exec"]')!
  assert.equal(execRow.querySelector('.bot-manager-disabled-tag'), null)
  assert.equal(execRow.classList.contains('manager-disabled'), false)
})

test('停派切換在 ⋯：advisor／executor／specialist 有，管理者與一般專案沒有', async () => {
  await mount(<Sidebar />)
  await settle()
  assert.equal(document.querySelector('.bot-manager-use-checkbox'), null, '側欄不再有勾選框')

  const open = async (id: string) => {
    await click(document.querySelector<HTMLButtonElement>(`[data-bot-id="${id}"] .head-menu-btn`)!)
    await settle()
    return menuItems().map((e) => e.textContent ?? '')
  }
  assert.ok((await open('b-adv')).includes('恢復派工'))
  assert.ok((await open('b-exec')).includes('停派'))
  assert.ok((await open('b-spec')).includes('停派'))
  assert.ok(!(await open('b-mgr')).some((x) => x === '停派' || x === '恢復派工'))
  assert.ok(!(await open('b-plain')).some((x) => x === '停派' || x === '恢復派工'))
  assert.ok(document.querySelector('[data-bot-id="b-exec"]')!.classList.contains('expert-member'))
  assert.equal(document.querySelector('[data-bot-id="b-mgr"]')!.classList.contains('expert-member'), false)
})

test('⋯ 的停派／恢復派工送 manager_use，不改選取；失敗不改狀態', async () => {
  await mount(<Sidebar />)
  await settle()
  assert.equal(useStore.getState().selectedBotId, 'b-mgr')

  await click(document.querySelector<HTMLButtonElement>('[data-bot-id="b-exec"] .head-menu-btn')!)
  await settle()
  await click(menuItems().find((b) => b.textContent === '停派')!)
  await settle()
  assert.equal(patchedBots.length, 1)
  assert.equal(patchedBots[0].id, 'b-exec')
  assert.equal(patchedBots[0].input.manager_use, false)
  assert.equal(useStore.getState().selectedBotId, 'b-mgr')
  patchedBots.length = 0

  await click(document.querySelector<HTMLButtonElement>('[data-bot-id="b-adv"] .head-menu-btn')!)
  await settle()
  await click(menuItems().find((b) => b.textContent === '恢復派工')!)
  await settle()
  assert.equal(patchedBots[0].id, 'b-adv')
  assert.equal(patchedBots[0].input.manager_use, true)
  assert.equal(useStore.getState().selectedBotId, 'b-mgr')

  const origPatch = useStore.getState().patchBot
  useStore.setState({
    patchBot: async () => {
      useStore.getState().notify('error', '網路錯誤')
      return null
    },
    bots: useStore.getState().bots.map((b) => (b.id === 'b-spec' ? { ...b, manager_use: false } : b)),
  })
  await settle()
  const specRow = document.querySelector<HTMLElement>('[data-bot-id="b-spec"]')!
  assert.ok(specRow.classList.contains('manager-disabled'))
  await click(specRow.querySelector<HTMLButtonElement>('.head-menu-btn')!)
  await settle()
  await click(menuItems().find((b) => b.textContent === '恢復派工')!)
  await settle()
  assert.ok(specRow.classList.contains('manager-disabled'), 'PATCH 失敗，列維持停派')
  useStore.setState({ patchBot: origPatch })
})
