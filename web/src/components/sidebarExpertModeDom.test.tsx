import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'
import type { Bot, Project } from '../api/types'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = (id: string, mode: 'parent_child' | 'expert' = 'parent_child') =>
  ({
    id,
    label: `proj-${id}`,
    path: `/${id}`,
    host: 'local',
    mode,
    workspace_id: null,
    github: null,
    created_at: new Date().toISOString(),
  }) as unknown as Project

const bot = (id: string, projectId: string, role?: Bot['role'], parent: string | null = null, rolePending = false) =>
  ({
    id,
    name: `bot-${id}`,
    project_id: projectId,
    kind: 'claude',
    role: role ?? null,
    role_pending: rolePending,
    identity: null,
    model: null,
    effort: null,
    args: [],
    env: {},
    autostart: false,
    inject_hooks: true,
    auto_approve: false,
    managed_by: parent ? 'child' : 'user',
    parent_bot_id: parent,
    primary: false,
    primary_position: 0,
    cwd: null,
    created_at: new Date().toISOString(),
  }) as unknown as Bot

beforeEach(() => {
  fakeApi()
})

test('專家模式專案出現段落標題、專案「專家」標籤與角色短標；parent_child 專案沒有', async () => {
  useStore.setState({
    projects: [
      project('p-pc', 'parent_child'),
      project('p-exp', 'expert'),
    ],
    bots: [
      // parent_child 專案
      bot('pc1', 'p-pc', 'manager'),
      bot('pc2', 'p-pc', 'executor'),
      // expert 專案
      bot('exp-mgr', 'p-exp', 'manager'),
      bot('exp-adv', 'p-exp', 'advisor'),
      bot('exp-exec', 'p-exp', 'executor', null, true), // role_pending
      bot('exp-none', 'p-exp', null),
      bot('exp-child', 'p-exp', null, 'exp-mgr'), // child
    ],
    runs: {},
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    connected: true,
    defaultConnected: true,
    socket: 'open',
    selectedBotId: 'exp-mgr',
    selectedProjectId: null,
    sidePanes: {},
    unownedPanes: [],
  } as never)

  await mount(<Sidebar />)
  await settle()

  // 1. parent_child 專案驗證
  const pcSection = document.querySelector<HTMLElement>('[data-project-id="p-pc"]')!
  assert.ok(pcSection, 'p-pc 專案區塊存在')
  assert.equal(pcSection.querySelectorAll('.project-expert-tag').length, 0, 'parent_child 專案標題不應有專家標籤')
  assert.equal(pcSection.querySelectorAll('.expert-section-head').length, 0, 'parent_child 專案不應有專家分段標題')
  assert.equal(pcSection.querySelectorAll('.bot-role-tag').length, 0, 'parent_child 專案 bot 列不應有角色短標')

  // 2. expert 專案驗證
  const expSection = document.querySelector<HTMLElement>('[data-project-id="p-exp"]')!
  assert.ok(expSection, 'p-exp 專案區塊存在')
  assert.equal(expSection.querySelectorAll('.project-expert-tag').length, 1, 'expert 專案標題應有專家標籤')

  // 分段標題驗證：順序 管理者 -> 顧問 -> 執行者 -> 未分配
  const headers = [...expSection.querySelectorAll('.expert-section-head')].map((el) => el.textContent?.trim())
  assert.deepEqual(headers, ['管理者1', '顧問1', '執行者1', '未分配1'], '專家分段標題順序與數量正確')

  // 頂層 bot 角色短標驗證
  const mgrTag = expSection.querySelector<HTMLElement>('[data-bot-id="exp-mgr"] .bot-role-tag')
  assert.ok(mgrTag, '管理者 bot 應有角色短標')
  assert.equal(mgrTag?.textContent?.trim(), '管', '管理者短標為「管」')

  const advTag = expSection.querySelector<HTMLElement>('[data-bot-id="exp-adv"] .bot-role-tag')
  assert.ok(advTag, '顧問 bot 應有角色短標')
  assert.equal(advTag?.textContent?.trim(), '顧', '顧問短標為「顧」')

  const execTag = expSection.querySelector<HTMLElement>('[data-bot-id="exp-exec"] .bot-role-tag')
  assert.ok(execTag, '執行者 bot 應有角色短標')
  assert.equal(execTag?.textContent?.trim(), '執', '執行者短標為「執」')
  assert.ok(execTag?.classList.contains('pending'), 'role_pending bot 角色標籤應有 pending class')
  assert.ok(execTag?.querySelector('.bot-role-dot'), 'role_pending bot 應有小提示點')
  assert.match(execTag?.getAttribute('title') ?? '', /待套用（需重啟）/, 'role_pending title 應提示待套用')

  // child bot 不應另外分段，且在 compact 子列中不獨立分段
  const childRow = expSection.querySelector<HTMLElement>('[data-bot-id="exp-child"]')
  assert.ok(childRow, 'child 列存在')
  assert.ok(childRow.closest('.bot-kids'), 'child 列應掛在父列底下')
})
