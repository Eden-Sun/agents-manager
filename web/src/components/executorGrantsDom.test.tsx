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
// store 原本的實作（setup 會換成假的）：正式錯誤 payload 的測試要走真的 api → ApiError → 錯誤文字。
const realSetExecutorGrants = useStore.getState().setExecutorGrants

function setup(grants: ExecutorGrant[] | undefined) {
  puts = []
  const project = { id: 'p-expert', label: '專家', path: '/e', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', executor_pool: [], ...(grants ? { executor_grants: grants, grants_revision: 'rev-1' } : {}) } as Project
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

/** 回指定 HTTP 狀態與 body 的 fetch（fakeApi 一律 200）。 */
function failingApi(status: number, body: unknown) {
  const reqs: { method: string; path: string; body: unknown }[] = []
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    reqs.push({ method: init?.method ?? 'GET', path: String(input), body: init?.body ? JSON.parse(init.body) : undefined })
    return init?.method === 'PUT' ? new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } }) : new Response('{}', { status: 200 })
  }) as unknown as typeof fetch
  return reqs
}

test('daemon 正式的 400／403／409 reason 都翻成人話，不露出原始錯誤碼', async () => {
  setup([GRANT])
  const notes: string[] = []
  useStore.setState({ setExecutorGrants: realSetExecutorGrants, notify: (_k: string, text: string) => void notes.push(text) } as never)
  const cases: [number, Record<string, unknown>, RegExp][] = [
    [400, { error: 'bad_request', reason: 'grant_specialist_track_missing', specialist: 'spA' }, /還沒有 track/],
    [400, { error: 'bad_request', reason: 'grant_specialist_role_invalid', specialist: 'ex1' }, /必須是這個專案的專家/],
    [400, { error: 'bad_request', reason: 'grant_executor_not_found', executor: 'zz' }, /只能授權這個專案的執行者/],
    [400, { error: 'bad_request', reason: 'grant_requires_expert_project' }, /只有專家模式/],
    [400, { error: 'bad_request', reason: 'executor_already_granted', executor: 'ex1', specialist: 'spA' }, /只能授權給一位專家/],
    [403, { error: 'forbidden', reason: 'grant_manager_only' }, /管理者能設定/],
    [409, { error: 'conflict', reason: 'grants_revision_conflict', revision: 'rev-2', grants: [] }, /已被他人修改，請重新整理/],
  ]
  for (const [status, body, want] of cases) {
    failingApi(status, body)
    notes.length = 0
    const ok = await useStore.getState().setExecutorGrants('p-expert', [{ specialist: 'spA', executor: 'ex1' }], 'rev-1')
    assert.equal(ok, false)
    assert.match(notes.join('\n'), want, String(body.reason))
    assert.doesNotMatch(notes.join('\n'), new RegExp(String(body.reason)), '不露出原始 reason')
  }
})

test('PUT 失敗（被他人修改 409）：草稿留著、仍是未儲存，送出的 body 帶 revision', async () => {
  setup([GRANT])
  useStore.setState({ setExecutorGrants: realSetExecutorGrants, notify: () => {} } as never)
  const reqs = failingApi(409, { error: 'conflict', reason: 'grants_revision_conflict', revision: 'rev-2', grants: [] })
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  await click(box('spB', 'ex2'))
  await settle()
  await click(button('儲存', document.querySelector('.project-savebar')!))
  await settle()
  const put = reqs.find((r) => r.method === 'PUT')!
  assert.deepEqual((put.body as { revision?: string }).revision, 'rev-1')
  assert.equal(box('spB', 'ex2').checked, true, '草稿沒被清掉')
  assert.match(document.querySelector('.project-savebar')?.textContent ?? '', /執行者授權/, '仍顯示未儲存的區塊')
})

test('PATCH 成功但授權 PUT 失敗：PATCH 那部分重設、授權草稿留著', async () => {
  setup([GRANT])
  let patched = 0
  useStore.setState({ patchProject: async () => ((patched += 1), true), setExecutorGrants: async () => false } as never)
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  await click(box('spB', 'ex2'))
  await settle()
  await click(button('儲存', document.querySelector('.project-savebar')!))
  await settle()
  assert.equal(box('spB', 'ex2').checked, true)
  assert.match(document.querySelector('.project-savebar')?.textContent ?? '', /執行者授權/)
  assert.equal(patched, 0, '只有授權有變：不送 PATCH')
})

test('mock：reason 與 daemon 一致、空 track 拒絕、重送既有授權保留原授權者、revision 過期 409', async () => {
  const { MockTransport } = await import('../api/mock')
  const m = new MockTransport() as unknown as { request: (method: string, path: string, body?: unknown) => Promise<unknown> }
  const state = (await m.request('GET', '/state')) as { projects: { id: string; mode: string; executor_grants: ExecutorGrant[]; grants_revision: string }[] }
  const p = state.projects.find((x) => x.mode === 'expert' && x.executor_grants.length > 0)
  assert.ok(p, 'mock 有預設授權的專家專案')
  const g = p.executor_grants[0]
  const reason = async (body: unknown) => {
    try {
      await m.request('PUT', `/projects/${p.id}/executor-grants`, body)
      return 'ok'
    } catch (e) {
      return String((e as { body?: { reason?: string } }).body?.reason ?? e)
    }
  }
  // 假裝 manager 改過：granted_by 不是 user，重送同一對要保留。
  ;(m as unknown as { projects: { id: string; executor_grants: ExecutorGrant[] }[] }).projects.find((x) => x.id === p.id)!.executor_grants[0].granted_by = 'mgr-bot'
  const out = (await m.request('PUT', `/projects/${p.id}/executor-grants`, { grants: [{ specialist: g.specialist, executor: g.executor }] })) as { grants: ExecutorGrant[]; revision: string }
  assert.equal(out.grants[0].granted_by, 'mgr-bot')
  assert.equal(out.grants[0].granted_at, g.granted_at)
  assert.equal(await reason({ grants: [], revision: 'stale' }), 'grants_revision_conflict')
  assert.equal(await reason({ grants: [{ specialist: 'nope', executor: g.executor }] }), 'grant_specialist_not_found')
  assert.equal(await reason({ grants: [{ specialist: g.executor, executor: g.executor }] }), 'grant_specialist_role_invalid')
  assert.equal(await reason({ grants: [{ specialist: g.specialist, executor: 'nope' }] }), 'grant_executor_not_found')
  ;(m as unknown as { bots: Bot[] }).bots.find((b) => b.id === g.specialist)!.track = ''
  assert.equal(await reason({ grants: [{ specialist: g.specialist, executor: g.executor }] }), 'grant_specialist_track_missing')
})
