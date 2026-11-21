import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { Profiler, useState, type ReactNode } from 'react'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Project } from '../api/types'
import { ProjectDeputyEditor } from './ProjectDeputyEditor'
import { ProjectSpecialistDispatchEditor } from './ProjectSpecialistDispatchEditor'
import { RolePolicyEditor } from './RolePolicyEditor'
import { NewBotForm } from './Sidebar'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

async function switchFrames<P>(opts: {
  first: P
  second: P
  render: (p: P) => ReactNode
  read: () => string
  settle?: () => Promise<void>
}) {
  const frames: string[] = []
  let set: (p: P) => void = () => {}
  function Harness() {
    const [p, setP] = useState(opts.first)
    set = setP
    return (
      <Profiler id="switch" onRender={() => frames.push(opts.read())}>
        {opts.render(p)}
      </Profiler>
    )
  }
  await mount(<Harness />)
  if (opts.settle) await opts.settle()
  const before = opts.read()
  frames.length = 0
  await act(async () => set(opts.second))
  await act(async () => new Promise<void>((resolve) => setImmediate(resolve)))
  return { before, frames: [...frames], final: opts.read() }
}

const button = (label: string) => [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === label)!

const deputyProject = (deputy: Project['deputy'] = null) =>
  ({ id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', deputy }) as unknown as Project
const bot = (id: string, role: Bot['role']) => ({ id, name: id, role, project_id: 'p1' }) as unknown as Bot
const DEPUTY_BOTS = [bot('adv', 'advisor'), bot('spc', 'specialist')]

test('副管理者：切到沒設副管理者的專案，第一幀不殘留舊的人選與門檻', async () => {
  useStore.setState({ projects: [deputyProject()], patchProject: async () => true } as never)
  const r = await switchFrames({
    first: deputyProject({ bot_id: 'adv', after_sec: 300 } as never),
    second: deputyProject(null),
    render: (p) => <ProjectDeputyEditor project={p} bots={DEPUTY_BOTS} />,
    read: () => `${document.querySelector<HTMLSelectElement>('select')?.value ?? ''}|${document.querySelector<HTMLInputElement>('input[type="number"]')?.value ?? ''}`,
  })
  assert.equal(r.before, 'adv|300', '切換前的確是舊值')
  assert.ok(r.frames.length > 0)
  assert.ok(r.frames.every((f) => f !== 'adv|300'), `切換後的 commit 出現舊值：${JSON.stringify(r.frames)}`)
  assert.equal(r.frames[0], r.final, '第一次 commit 就是最後的狀態')
})

const dispatchProject = (specialist_dispatch: boolean) =>
  ({ id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', specialist_dispatch }) as unknown as Project

test('專家派工開關：切到另一個專案，第一幀不顯示舊的勾選與「可儲存」', async () => {
  useStore.setState({ projects: [dispatchProject(false)], patchProject: async () => true } as never)
  const r = await switchFrames({
    first: dispatchProject(false),
    second: dispatchProject(true),
    render: (p) => <ProjectSpecialistDispatchEditor project={p} />,
    read: () => `${document.querySelector<HTMLInputElement>('input[type="checkbox"]')?.checked}|save-disabled=${button('儲存')?.disabled}`,
  })
  assert.equal(r.before, 'false|save-disabled=true')
  assert.ok(r.frames.every((f) => f !== 'false|save-disabled=false'), `切換後的 commit 出現舊的勾選與可存狀態：${JSON.stringify(r.frames)}`)
  assert.equal(r.frames[0], r.final)
})

const roleProject = (role_policy: Project['role_policy']) =>
  ({ id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '', role_policy }) as unknown as Project

test('角色設定：切到另一個專案，第一幀不顯示舊草稿造成的「已變更」', async () => {
  useStore.setState({ projects: [roleProject({})], patchProject: async () => true } as never)
  const r = await switchFrames({
    first: roleProject({ manager: { context_tokens: 300_000 } }),
    second: roleProject({}),
    render: (p) => <RolePolicyEditor project={p} />,
    read: () => document.querySelector('.rp-savebar .hint')?.textContent ?? '',
  })
  assert.equal(r.before, '沒有變更', '切換前草稿等於儲存值')
  assert.ok(r.frames.every((f) => !f.startsWith('已變更')), `切換後的 commit 出現舊草稿的「已變更」：${JSON.stringify(r.frames)}`)
  assert.equal(r.frames[0], r.final)
})

test('新增 Bot 表單：點選另一個專案後，第一幀不殘留上一個專案的預設名稱', async () => {
  resetStoreForTest()
  const p = (id: string) => ({ id, label: id, path: `/${id}`, host: 'local', mode: 'parent_child', workspace_id: null, github: null, created_at: '' })
  useStore.setState({
    projects: [p('p1'), p('p2')],
    bots: [{ id: 'b1', name: 'claude-1', kind: 'claude', project_id: 'p1', role: 'executor', managed_by: 'user' }],
    drafts: {},
  } as never)
  const readForm = () => [...document.querySelectorAll<HTMLInputElement | HTMLSelectElement>('input, select')].map((e) => e.value).join('|')
  const frames: string[] = []
  const Probe = () => (
    <Profiler id="newbot" onRender={() => frames.push(readForm())}>
      <NewBotForm onDone={() => {}} />
    </Profiler>
  )
  await mount(<Probe />)
  const start = readForm()
  frames.length = 0
  const option = [...document.querySelectorAll<HTMLButtonElement>('button.opt')].find((b) => b.textContent?.includes('p2'))!
  await click(option)
  await act(async () => new Promise<void>((resolve) => setImmediate(resolve)))
  const final = readForm()
  assert.notEqual(final, start, '切換應該改變表單內容（否則這條測不到東西）')
  assert.ok(frames.every((f) => f !== start), `切換後的 commit 出現上一個專案的表單值：${JSON.stringify(frames)}`)
  assert.equal(frames[0], final, '第一次 commit 就是最後的狀態')
})
