import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { toProject } from '../api/normalize'
import { ProjectExpertSettings } from './ProjectExpertSettings'
import { ProjectSpecialistDispatchEditor } from './ProjectSpecialistDispatchEditor'
import type { Bot, PatchProjectInput, Project } from '../api/types'

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

let patches: PatchProjectInput[] = []

function setup(dispatch: boolean | undefined) {
  patches = []
  const project = { id: 'p-expert', label: '專家', path: '/e', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', ...(dispatch === undefined ? {} : { specialist_dispatch: dispatch }) } as Project
  useStore.setState({
    models: {},
    modelsFailedAt: {},
    patchProject: async (_id: string, input: PatchProjectInput) => {
      patches.push(input)
      useStore.setState((s) => ({ projects: s.projects.map((p) => (p.id === 'p-expert' ? { ...p, ...input } : p)) }) as never)
      return true
    },
    projects: [project],
    bots: [bot('mgr', 'manager'), bot('spA', 'specialist', 'infra'), bot('ex1', 'executor'), bot('ex2', 'executor')],
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

const checkbox = () => document.querySelector<HTMLInputElement>('.project-specialist-dispatch-editor input[type="checkbox"]')

function button(text: string, root: ParentNode = document): HTMLButtonElement {
  const b = Array.from(root.querySelectorAll<HTMLButtonElement>('button')).find((x) => x.textContent?.trim() === text)
  assert.ok(b, `應有按鈕「${text}」`)
  return b
}

beforeEach(() => {
  fakeApi()
})

test('專家模式設定有一個勾選「專家可直接派工給執行者」，預設開、附一行說明，沒有逐格授權', async () => {
  setup(true)
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  const box = checkbox()
  assert.ok(box, '有開關')
  assert.equal(box.checked, true)
  const editor = document.querySelector('.project-specialist-dispatch-editor')!
  assert.match(editor.textContent ?? '', /專家可直接派工給執行者/)
  assert.match(editor.querySelector('.hint')?.textContent ?? '', /關掉就只有管理者能派/)
  assert.equal(document.querySelectorAll('.grants-row, .grants-exec').length, 0, '沒有逐格授權矩陣')
})

test('取消勾選後跟著底部「儲存」送 PATCH {specialist_dispatch:false}；存完回到沒有變更', async () => {
  setup(true)
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  await click(checkbox()!)
  await settle()
  assert.match(document.querySelector('.project-savebar')?.textContent ?? '', /專家派工/)
  await click(button('儲存', document.querySelector('.project-savebar')!))
  await settle()
  assert.deepEqual(patches, [{ specialist_dispatch: false }])
  assert.equal(checkbox()!.checked, false)
  assert.match(document.querySelector('.project-savebar')?.textContent ?? '', /沒有變更/)
})

test('單獨掛載時有自己的儲存鈕；改回原值就不能存', async () => {
  setup(false)
  await mount(<ProjectSpecialistDispatchEditor project={useStore.getState().projects[0]} />)
  await settle()
  const save = button('儲存')
  assert.equal(checkbox()!.checked, false)
  assert.equal(save.disabled, true)
  await click(checkbox()!)
  await settle()
  assert.equal(save.disabled, false)
  await click(checkbox()!)
  await settle()
  assert.equal(save.disabled, true, '改回原值')
  await click(checkbox()!)
  await settle()
  await click(save)
  await settle()
  assert.deepEqual(patches, [{ specialist_dispatch: true }])
})

test('舊 daemon 沒有 specialist_dispatch：不畫開關，其他區塊照常', async () => {
  setup(undefined)
  await mount(<ProjectExpertSettings projectId="p-expert" />)
  await settle()
  assert.equal(checkbox(), null)
  assert.ok(document.querySelector('.project-deputy-editor'), '副管理者區塊照常')
})

test('normalize：daemon 的 specialist_dispatch 原樣帶入，缺欄位為 undefined', () => {
  const base = { id: 'p', path: '/p', label: 'p', host: 'local', mode: 'expert', created_at: '' }
  assert.equal(toProject({ ...base, specialist_dispatch: false })?.specialist_dispatch, false)
  assert.equal(toProject({ ...base, specialist_dispatch: true })?.specialist_dispatch, true)
  assert.equal(toProject(base)?.specialist_dispatch, undefined)
})
