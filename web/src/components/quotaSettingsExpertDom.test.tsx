import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'
import { ProjectExpertSettings } from './ProjectExpertSettings'
import type { PatchBotInput, PatchProjectInput } from '../api/types'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

let patchedBots: { id: string; input: PatchBotInput }[] = []
let patchedProjects: { id: string; input: PatchProjectInput }[] = []

beforeEach(() => {
  fakeApi()
  patchedBots = []
  patchedProjects = []

  const mockPatchBot = async (id: string, input: PatchBotInput) => {
    patchedBots.push({ id, input })
    useStore.setState((s) => ({
      bots: s.bots.map((b) => (b.id === id ? { ...b, ...input } : b)),
    }))
    return { needsRestart: false, deferred: false, remappedModel: null }
  }

  const mockPatchProject = async (id: string, input: PatchProjectInput) => {
    patchedProjects.push({ id, input })
    useStore.setState((s) => ({
      projects: s.projects.map((p) => (p.id === id ? { ...p, ...input } : p)),
    }))
    return true
  }

  useStore.setState({
    patchBot: mockPatchBot,
    patchProject: mockPatchProject,
    projects: [
      {
        id: 'p-expert',
        label: '專家專案',
        path: '/expert',
        host: 'local',
        mode: 'expert',
        executor_pool: [
          { id: 'codex-luna-xhigh-256k', kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 2 },
          { id: 'claude-haiku-low-200k', kind: 'claude', model: 'haiku', effort: 'low', context_tokens: 200000, count: 1 },
        ],
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

test('專家模式側欄：manager_use=false 的 bot 顯示「停用」小標記與列名淡化樣式', async () => {
  await mount(<Sidebar />)
  await settle()

  const advRow = document.querySelector<HTMLElement>('[data-bot-id="b-adv"]')!
  assert.ok(advRow, '應找到 adv-1 的列')
  const tag = advRow.querySelector('.bot-manager-disabled-tag')
  assert.ok(tag, '應有停用標記')
  assert.equal(tag.textContent, '停用')
  assert.ok(advRow.classList.contains('manager-disabled'), '停用的列應有 manager-disabled class')

  const execRow = document.querySelector<HTMLElement>('[data-bot-id="b-exec"]')!
  assert.equal(execRow.querySelector('.bot-manager-disabled-tag'), null, 'exec-1 啟用中不應有停用標記')
  assert.equal(execRow.classList.contains('manager-disabled'), false, 'exec-1 啟用中不應有 manager-disabled class')
})

test('側欄勾選框：只在專家模式 advisor/executor/specialist 出現，管理者與一般專案不出現', async () => {
  await mount(<Sidebar />)
  await settle()

  // 1. 專家模式的 advisor / executor / specialist 列應有勾選框
  const advRow = document.querySelector<HTMLElement>('[data-bot-id="b-adv"]')!
  const execRow = document.querySelector<HTMLElement>('[data-bot-id="b-exec"]')!
  const specRow = document.querySelector<HTMLElement>('[data-bot-id="b-spec"]')!

  const advBox = advRow.querySelector<HTMLInputElement>('.bot-manager-use-checkbox')
  const execBox = execRow.querySelector<HTMLInputElement>('.bot-manager-use-checkbox')
  const specBox = specRow.querySelector<HTMLInputElement>('.bot-manager-use-checkbox')

  assert.ok(advBox, 'advisor 應有勾選框')
  assert.ok(execBox, 'executor 應有勾選框')
  assert.ok(specBox, 'specialist 應有勾選框')

  assert.equal(advBox.checked, false, 'adv-1 manager_use 為 false')
  assert.equal(execBox.checked, true, 'exec-1 manager_use 為 true')
  assert.equal(specBox.checked, true, 'spec-1 manager_use 為 true')

  // aria-label 與 title
  const expectedTitle = '管理者可用：開啟後管理者可派工、啟動或重啟此成員'
  assert.equal(advBox.getAttribute('aria-label'), expectedTitle)
  assert.equal(advBox.getAttribute('title'), expectedTitle)

  // 2. 管理者那一列不出現
  const mgrRow = document.querySelector<HTMLElement>('[data-bot-id="b-mgr"]')!
  assert.equal(mgrRow.querySelector('.bot-manager-use-checkbox'), null, '管理者列不應有勾選框')

  // 3. 一般模式專案不出現
  const plainRow = document.querySelector<HTMLElement>('[data-bot-id="b-plain"]')!
  assert.equal(plainRow.querySelector('.bot-manager-use-checkbox'), null, '一般專案不應有勾選框')
})

test('側欄勾選框：點擊切換狀態並送出 PATCH，點擊不選取該 bot，失敗還原', async () => {
  await mount(<Sidebar />)
  await settle()

  const initialSelected = useStore.getState().selectedBotId
  assert.equal(initialSelected, 'b-mgr')

  const execRow = document.querySelector<HTMLElement>('[data-bot-id="b-exec"]')!
  const execBox = execRow.querySelector<HTMLInputElement>('.bot-manager-use-checkbox')!

  // 點擊勾選框
  await click(execBox)
  await settle()

  // 1. 不會選取該 bot
  assert.equal(useStore.getState().selectedBotId, 'b-mgr', '點勾選框不應改變選取的 bot')

  // 2. 送出 PATCH
  assert.equal(patchedBots.length, 1)
  assert.equal(patchedBots[0].id, 'b-exec')
  assert.equal(patchedBots[0].input.manager_use, false)

  // 3. 測試失敗還原
  const origPatch = useStore.getState().patchBot
  useStore.setState({
    patchBot: async () => {
      useStore.getState().notify('error', '網路錯誤')
      return null
    },
  })

  const specRow = document.querySelector<HTMLElement>('[data-bot-id="b-spec"]')!
  const specBox = specRow.querySelector<HTMLInputElement>('.bot-manager-use-checkbox')!
  assert.equal(specBox.checked, true)

  await click(specBox)
  await settle()

  // 失敗後還原為 true
  assert.equal(specBox.checked, true, 'PATCH 失敗應還原狀態')
  useStore.setState({ patchBot: origPatch })
})

test('bot 列 ⋯ 選單：切換「管理者可用」勾選送出 PATCH /api/bots/{id}', async () => {
  await mount(<Sidebar />)
  await settle()

  // exec-1 目前 manager_use=true，選單應有「☑ 管理者可用」
  const execRow = document.querySelector<HTMLElement>('[data-bot-id="b-exec"]')!
  const menuBtn = execRow.querySelector<HTMLButtonElement>('.bot-actions .head-menu-btn')!
  await click(menuBtn)
  await settle()

  const disableBtn = Array.from(document.querySelectorAll<HTMLButtonElement>('.head-menu-item')).find(
    (b) => b.textContent?.includes('管理者可用'),
  )
  assert.ok(disableBtn, '應有「管理者可用」選項')
  assert.match(disableBtn!.textContent ?? '', /☑/, '啟用中應顯示 ☑')
  await click(disableBtn!)
  await settle()

  assert.equal(patchedBots.length, 1)
  assert.equal(patchedBots[0].id, 'b-exec')
  assert.equal(patchedBots[0].input.manager_use, false)

  // adv-1 目前 manager_use=false，打開選單應有「☐ 管理者可用」
  const advRow = document.querySelector<HTMLElement>('[data-bot-id="b-adv"]')!
  const advMenuBtn = advRow.querySelector<HTMLButtonElement>('.bot-actions .head-menu-btn')!
  await click(advMenuBtn)
  await settle()

  const enableBtn = Array.from(document.querySelectorAll<HTMLButtonElement>('.head-menu-item')).find(
    (b) => b.textContent?.includes('管理者可用'),
  )
  assert.ok(enableBtn, '應有「管理者可用」選項')
  assert.match(enableBtn!.textContent ?? '', /☐/, '停用中應顯示 ☐')
  await click(enableBtn!)
  await settle()

  assert.equal(patchedBots.length, 2)
  assert.equal(patchedBots[1].id, 'b-adv')
  assert.equal(patchedBots[1].input.manager_use, true)
})

test('專案設定 ProjectExpertSettings：條目改上限後儲存，送出整份條目陣列', async () => {
  fakeApi()
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()

  const rows = document.querySelectorAll<HTMLElement>('.executor-pool-entry')
  assert.equal(rows.length, 2)
  assert.equal(rows[0].dataset.poolId, 'codex-luna-xhigh-256k')
  assert.match(rows[0].textContent ?? '', /codex/)
  assert.match(rows[0].textContent ?? '', /256k/)

  const countInput = rows[0].querySelector<HTMLInputElement>('input[type="number"]')!
  assert.equal(countInput.value, '2')
  await typeInto(countInput, '3')
  await settle()

  const saveBtn = Array.from(document.querySelectorAll<HTMLButtonElement>('button')).find(
    (b) => b.textContent?.includes('儲存執行者池'),
  )!
  assert.equal(saveBtn.disabled, false)
  await click(saveBtn)
  await settle()

  assert.equal(patchedProjects.length, 1)
  assert.equal(patchedProjects[0].id, 'p-expert')
  assert.deepEqual(patchedProjects[0].input.executor_pool, [
    { id: 'codex-luna-xhigh-256k', kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 3 },
    { id: 'claude-haiku-low-200k', kind: 'claude', model: 'haiku', effort: 'low', context_tokens: 200000, count: 1 },
  ])
})
