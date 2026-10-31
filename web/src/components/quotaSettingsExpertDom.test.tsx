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
        executor_pool: { codex: 2, claude: 1 },
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

test('專家模式側欄：manager_use=false 的 bot 顯示「停用」小標記', async () => {
  await mount(<Sidebar />)
  await settle()

  const advRow = document.querySelector<HTMLElement>('[data-bot-id="b-adv"]')!
  assert.ok(advRow, '應找到 adv-1 的列')
  const tag = advRow.querySelector('.bot-manager-disabled-tag')
  assert.ok(tag, '應有停用標記')
  assert.equal(tag.textContent, '停用')

  const execRow = document.querySelector<HTMLElement>('[data-bot-id="b-exec"]')!
  assert.equal(execRow.querySelector('.bot-manager-disabled-tag'), null, 'exec-1 啟用中不應有停用標記')
})

test('bot 列 ⋯ 選單：切換「禁止管理者使用」／「允許管理者使用」送出 PATCH /api/bots/{id}', async () => {
  await mount(<Sidebar />)
  await settle()

  // exec-1 目前 manager_use=true，選單應有「禁止管理者使用」
  const execRow = document.querySelector<HTMLElement>('[data-bot-id="b-exec"]')!
  const menuBtn = execRow.querySelector<HTMLButtonElement>('.bot-actions .head-menu-btn')!
  await click(menuBtn)
  await settle()

  const disableBtn = Array.from(document.querySelectorAll<HTMLButtonElement>('.head-menu-item')).find(
    (b) => b.textContent?.includes('禁止管理者使用'),
  )
  assert.ok(disableBtn, '應有「禁止管理者使用」按鈕')
  await click(disableBtn!)
  await settle()

  assert.equal(patchedBots.length, 1)
  assert.equal(patchedBots[0].id, 'b-exec')
  assert.equal(patchedBots[0].input.manager_use, false)

  // adv-1 目前 manager_use=false，打開選單應有「允許管理者使用」
  const advRow = document.querySelector<HTMLElement>('[data-bot-id="b-adv"]')!
  const advMenuBtn = advRow.querySelector<HTMLButtonElement>('.bot-actions .head-menu-btn')!
  await click(advMenuBtn)
  await settle()

  const enableBtn = Array.from(document.querySelectorAll<HTMLButtonElement>('.head-menu-item')).find(
    (b) => b.textContent?.includes('允許管理者使用'),
  )
  assert.ok(enableBtn, '應有「允許管理者使用」按鈕')
  await click(enableBtn!)
  await settle()

  assert.equal(patchedBots.length, 2)
  assert.equal(patchedBots[1].id, 'b-adv')
  assert.equal(patchedBots[1].input.manager_use, true)
})

test('專案設定 ProjectExpertSettings：編輯執行者池並儲存送出 PATCH /api/projects/{id}', async () => {
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()

  const codexInput = document.querySelector<HTMLInputElement>('.project-executor-pool-row input[min="0"]')!
  assert.ok(codexInput, '應找到執行者池輸入框')

  const inputs = Array.from(document.querySelectorAll<HTMLInputElement>('.project-executor-pool-row input'))
  // BOT_KINDS: claude, codex, grok, agy
  // claude = 1, codex = 2
  assert.equal(inputs[0].value, '1') // claude
  assert.equal(inputs[1].value, '2') // codex

  // 修改 grok 為 3
  await typeInto(inputs[2], '3')
  await settle()

  const saveBtn = Array.from(document.querySelectorAll<HTMLButtonElement>('button')).find(
    (b) => b.textContent?.includes('儲存執行者池'),
  )!
  assert.ok(saveBtn, '應有儲存執行者池按鈕')
  assert.equal(saveBtn.disabled, false)

  await click(saveBtn)
  await settle()

  assert.equal(patchedProjects.length, 1)
  assert.equal(patchedProjects[0].id, 'p-expert')
  assert.deepEqual(patchedProjects[0].input.executor_pool, {
    claude: 1,
    codex: 2,
    grok: 3,
  })
})
