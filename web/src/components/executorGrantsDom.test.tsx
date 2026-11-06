import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import * as api from '../api'
import { toExecutorGrants } from '../api/normalize'
import { ProjectExpertSettings } from './ProjectExpertSettings'
import { ProjectExecutorGrantsEditor } from './ProjectExecutorGrantsEditor'
import { Sidebar } from './Sidebar'
import type { Bot, ExecutorGrant, ExecutorGrantInput, Project } from '../api/types'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const bot = (id: string, role: Bot['role'], track: string | null = null) =>
  ({
    id,
    name: `bot-${id}`,
    project_id: 'p-expert',
    kind: 'claude',
    role,
    track,
    role_pending: false,
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
    created_at: new Date().toISOString(),
  }) as unknown as Bot

const GRANT: ExecutorGrant = { specialist: 'spA', executor: 'ex1', track: 'infra', granted_by: 'user', granted_at: '2026-11-06T02:28:00Z' }

let puts: ExecutorGrantInput[][] = []

function setup(grants: ExecutorGrant[] | undefined) {
  puts = []
  const project = { id: 'p-expert', label: '專家', path: '/e', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', executor_pool: [], ...(grants ? { executor_grants: grants } : {}) } as Project
  useStore.setState({
    models: {},
    modelsFailedAt: {},
    patchProject: async () => true,
    setExecutorGrants: async (_id: string, input: ExecutorGrantInput[]) => {
      puts.push(input)
      const next = input.map((g) => ({ ...g, track: 'x', granted_by: 'user', granted_at: '2026-11-06T03:00:00Z' }))
      useStore.setState((s) => ({ projects: s.projects.map((p) => (p.id === 'p-expert' ? { ...p, executor_grants: next } : p)) }))
      return true
    },
    projects: [project],
    bots: [bot('mgr', 'manager'), bot('spA', 'specialist', 'infra'), bot('spB', 'specialist', 'billing'), bot('ex1', 'executor'), bot('ex2', 'executor')],
    connected: true,
    defaultConnected: true,
    socket: 'open',
    runs: {},
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    busy: {},
  } as never)
}

const box = (spec: string, exec: string) =>
  document.querySelector<HTMLInputElement>(`.grants-row[data-specialist="${spec}"] .grants-exec[data-executor="${exec}"] input`)!
const execLabel = (spec: string, exec: string) => document.querySelector<HTMLLabelElement>(`.grants-row[data-specialist="${spec}"] .grants-exec[data-executor="${exec}"]`)!

function button(text: string, root: ParentNode = document): HTMLButtonElement {
  const b = Array.from(root.querySelectorAll<HTMLButtonElement>('button')).find((x) => x.textContent?.trim() === text)
  assert.ok(b, `應有按鈕「${text}」`)
  return b
}

beforeEach(() => {
  fakeApi()
})

test('每位專家一列；已授權給別位專家的執行者不可選並標「已授權給」；顯示授權者', async () => {
  setup([GRANT])
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  assert.equal(document.querySelectorAll('.grants-row').length, 2, '兩位專家兩列')
  assert.equal(box('spA', 'ex1').checked, true)
  assert.equal(box('spA', 'ex1').disabled, false)
  assert.match(execLabel('spA', 'ex1').textContent ?? '', /使用者 授權/)
  // spB 那列：ex1 已授權給 spA → 不可選
  assert.equal(box('spB', 'ex1').disabled, true)
  assert.equal(box('spB', 'ex1').checked, false)
  assert.match(execLabel('spB', 'ex1').textContent ?? '', /已授權給 bot-spA/)
  assert.equal(box('spB', 'ex2').disabled, false)
  // 管理者、專家不出現在可勾的執行者裡
  assert.equal(document.querySelector('.grants-exec[data-executor="mgr"]'), null)
})

test('勾選後跟著底部「儲存」送出 PUT 整組；取消原授權後別位專家可以選', async () => {
  setup([GRANT])
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  await click(box('spB', 'ex2'))
  await settle()
  assert.match(document.querySelector('.project-savebar')?.textContent ?? '', /執行者授權/)
  // 取消 spA→ex1 後，spB 那格 ex1 解除停用
  await click(box('spA', 'ex1'))
  await settle()
  assert.equal(box('spB', 'ex1').disabled, false)
  await click(box('spB', 'ex1'))
  await settle()
  assert.equal(box('spA', 'ex1').disabled, true, '反過來 spA 那格變不可選')
  await click(button('儲存', document.querySelector('.project-savebar')!))
  await settle()
  assert.equal(puts.length, 1)
  const sorted = [...puts[0]].sort((a, b) => a.executor.localeCompare(b.executor))
  assert.deepEqual(sorted, [
    { specialist: 'spB', executor: 'ex1' },
    { specialist: 'spB', executor: 'ex2' },
  ])
  assert.match(document.querySelector('.project-savebar')?.textContent ?? '', /沒有變更/)
})

test('只改授權不送 PATCH；單獨掛載時有自己的儲存鈕', async () => {
  setup([])
  let patched = 0
  useStore.setState({ patchProject: async () => ((patched += 1), true) } as never)
  const project = useStore.getState().projects[0]
  await mount(<ProjectExecutorGrantsEditor project={project} bots={useStore.getState().bots} />)
  await settle()
  const save = button('儲存')
  assert.equal(save.disabled, true)
  await click(box('spA', 'ex2'))
  await settle()
  assert.equal(save.disabled, false)
  await click(save)
  await settle()
  assert.deepEqual(puts, [[{ specialist: 'spA', executor: 'ex2' }]])
  assert.equal(patched, 0)
})

test('舊 daemon 沒有 executor_grants：顯示提示、不畫勾選框，其他區塊照常', async () => {
  setup(undefined)
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  assert.match(document.querySelector('.project-grants-editor')?.textContent ?? '', /daemon 版本太舊/)
  assert.equal(document.querySelectorAll('.grants-exec input').length, 0)
  assert.ok(document.querySelector('.project-deputy-editor'), '副管理者區塊照常')
})

test('側欄：被授權的執行者名稱旁小字「→ 專家名」，其他列沒有', async () => {
  setup([GRANT])
  await mount(<Sidebar />)
  await settle()
  const row = (id: string) => document.querySelector(`.bot-row[data-bot-id="${id}"]`)!
  assert.equal(row('ex1').querySelector('.bot-grant-mark')?.textContent, '→ bot-spA')
  assert.equal(row('ex2').querySelector('.bot-grant-mark'), null)
  assert.equal(row('spA').querySelector('.bot-grant-mark'), null)
})

test('api.setExecutorGrants 打 PUT /projects/:id/executor-grants，body 是 {grants}', async () => {
  const reqs = fakeApi(() => ({ grants: [{ ...GRANT, specialist: 'spB' }] }))
  const out = await api.setExecutorGrants('p-expert', [{ specialist: 'spB', executor: 'ex1' }])
  const put = reqs.find((r) => r.method === 'PUT')!
  assert.match(put.path, /\/projects\/p-expert\/executor-grants$/)
  assert.deepEqual(put.body, { grants: [{ specialist: 'spB', executor: 'ex1' }] })
  assert.equal(out[0].specialist, 'spB')
  assert.deepEqual(toExecutorGrants([{ specialist: 'a' }, 'x', GRANT]), [GRANT], '缺欄位的項丟掉')
})
