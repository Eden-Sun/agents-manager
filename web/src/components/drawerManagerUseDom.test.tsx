/** 手機抽屜：點專家成員／群集列左邊的「管理者可用」勾選框，只切換停用狀態，不選 bot、不收抽屜（使用者 2026-11-04）。 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, type FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { drawerShouldClose } from '../lib/drawerNav'
import type { Bot, Project } from '../api/types'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project: Project = { id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', executor_pool: [], workspace_id: null, github: null, created_at: '' } as unknown as Project

const bot = (id: string, over: Partial<Bot> = {}): Bot =>
  ({
    id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: null, model: 'haiku', effort: null,
    args: [], env: {}, autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user',
    parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
    role: 'executor', manager_use: true, track: null, runtime_override: null, cluster: null, ...over,
  }) as unknown as Bot

let requests: FakeRequest[] = []

beforeEach(() => {
  try { window.localStorage.clear() } catch { /* ignore */ }
  requests = fakeApi((req) => (req.method === 'PATCH' ? { needs_restart: false } : undefined))
  useStore.setState({
    projects: [project],
    bots: [bot('m', { role: 'manager' }), bot('a', { role: 'advisor' }), bot('x1', { cluster: 'haiku' }), bot('x2', { cluster: 'haiku' })],
    runs: {},
    selectedProjectId: 'p1',
    selectedBotId: 'm',
    busy: {},
  } as never)
})

/** App 的抽屜外殼（同一份 drawerShouldClose）：開著時點到導覽目標就收。 */
function Shell() {
  const [open, setOpen] = useState(true)
  return (
    <aside className={`sidebar${open ? ' open' : ''}`} data-open={open ? '1' : '0'} onClickCapture={(e) => { if (open && drawerShouldClose(e.target)) setOpen(false) }}>
      <Sidebar />
    </aside>
  )
}
const isOpen = () => document.querySelector('aside.sidebar')!.getAttribute('data-open') === '1'
const rowOf = (id: string) => document.querySelector<HTMLElement>(`.bot-row[data-bot-id="${id}"]`)!

test('點成員列的勾選框：抽屜不收、選取不變、PATCH manager_use=false', async () => {
  await mount(<Shell />)
  await settle()
  const box = rowOf('a').querySelector<HTMLInputElement>('.bot-manager-use-checkbox')!
  assert.ok(box)
  await click(box)
  await settle()
  assert.equal(isOpen(), true, '抽屜仍開著')
  assert.equal(useStore.getState().selectedBotId, 'm', '沒選到該 bot')
  const patches = requests.filter((r) => r.method === 'PATCH')
  assert.equal(patches.length, 1)
  assert.equal((patches[0].body as { manager_use?: boolean }).manager_use, false)
})

test('點勾選框外的 label（觸控範圍）：同樣不收抽屜', async () => {
  await mount(<Shell />)
  await settle()
  await click(rowOf('a').querySelector('.bot-manager-use-wrap')!)
  await settle()
  assert.equal(isOpen(), true)
  assert.equal(useStore.getState().selectedBotId, 'm')
})

test('群集列的整組勾選框：不收抽屜', async () => {
  await mount(<Shell />)
  await settle()
  const box = document.querySelector<HTMLInputElement>('.bot-cluster-row .bot-manager-use-checkbox')!
  assert.ok(box)
  await click(box)
  await settle()
  assert.equal(isOpen(), true)
  assert.equal(useStore.getState().selectedBotId, 'm')
})

test('對照：點 bot 列本身仍然收抽屜並選取', async () => {
  await mount(<Shell />)
  await settle()
  await click(rowOf('a'))
  await settle()
  assert.equal(isOpen(), false)
  assert.equal(useStore.getState().selectedBotId, 'a')
})
