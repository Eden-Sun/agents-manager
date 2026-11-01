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
      { ...bot('exp-spec', 'p-exp', 'specialist'), track: 'infra' } as Bot,
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
  assert.deepEqual(headers, ['管理者', '專家', '顧問', '執行者', '未分配'], '專家分段標題順序正確（不帶人數）')

  // 分段標題已寫角色：列上不再重複角色小標（2026-11-01 使用者），只有待套用的留著
  for (const id of ['exp-mgr', 'exp-adv', 'exp-spec']) {
    assert.equal(expSection.querySelector(`[data-bot-id="${id}"] .bot-role-tag`), null, `${id} 不應有角色短標`)
  }
  assert.equal(expSection.querySelector('[data-bot-id="exp-spec"] .bot-track-tag')?.textContent?.trim(), 'infra', '專家顯示 track')
  assert.equal(expSection.querySelector('[data-bot-id="exp-adv"] .bot-track-tag'), null, '非專家不顯示 track')

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

test('副管理者：只有專案設定的那位成員多一顆「副」小標；沒設副管理者的專案沒有', async () => {
  useStore.setState({
    projects: [
      { ...project('p-dep', 'expert'), deputy: { bot_id: 'dep-adv', after_sec: 120 } } as Project,
      project('p-nodep', 'expert'),
    ],
    bots: [
      bot('dep-mgr', 'p-dep', 'manager'),
      bot('dep-adv', 'p-dep', 'advisor'),
      bot('dep-spc', 'p-dep', 'advisor'),
      bot('nodep-mgr', 'p-nodep', 'manager'),
      bot('nodep-adv', 'p-nodep', 'advisor'),
    ],
    runs: {},
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    connected: true,
    defaultConnected: true,
    socket: 'open',
    selectedBotId: 'dep-mgr',
    selectedProjectId: null,
    sidePanes: {},
    unownedPanes: [],
  } as never)

  await mount(<Sidebar />)
  await settle()

  const tag = document.querySelector<HTMLElement>('[data-bot-id="dep-adv"] .bot-deputy-tag')
  assert.ok(tag, '副管理者有小標')
  assert.equal(tag?.textContent?.trim(), '副')
  assert.match(tag?.getAttribute('title') ?? '', /120 秒/, 'tooltip 寫明門檻')
  assert.equal(document.querySelectorAll('.bot-deputy-tag').length, 1, '其他成員、其他專案都沒有')
})
