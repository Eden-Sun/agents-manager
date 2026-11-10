/** 專家模式側欄執行者群集列：分組、計數、展開、整組勾選、半勾。 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, type FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import type { Bot, Project } from '../api/types'
import { Sidebar } from './Sidebar'
import { groupClusterItems } from '../lib/expertClusters'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project: Project = { id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '' } as unknown as Project

const bot = (id: string, over: Partial<Bot> = {}): Bot =>
  ({
    id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: null, model: 'haiku', effort: null,
    args: [], env: {}, autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user',
    parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
    role: 'executor', manager_use: true, track: null, runtime_override: null, cluster: null, ...over,
  }) as unknown as Bot

const run = (status: 'idle' | 'working') => ({ id: 'r', state: 'running', agent_status: status }) as never

let requests: FakeRequest[] = []

function seed(managerUse: boolean[] = [true, true, true]) {
  useStore.setState({
    projects: [project],
    bots: [
      bot('m', { role: 'manager', cluster: null }),
      bot('h1', { cluster: 'haiku', manager_use: managerUse[0] }),
      bot('h2', { cluster: 'haiku', manager_use: managerUse[1] }),
      bot('h3', { cluster: 'haiku', manager_use: managerUse[2] }),
      bot('solo', { kind: 'grok', model: null, cluster: null }),
    ],
    runs: { h1: run('idle'), h2: run('working'), h3: run('idle') },
    selectedProjectId: 'p1',
    selectedBotId: 'm',
    busy: {},
    notices: [],
  } as never)
}

beforeEach(() => {
  try { window.localStorage.clear() } catch { /* ignore */ }
  requests = fakeApi((req) => (req.method === 'PATCH' ? { needs_restart: false } : undefined))
})

const clusterRow = () => document.querySelector<HTMLElement>('.bot-cluster-row')
const rowIds = () => [...document.querySelectorAll<HTMLElement>('.bot-row[data-bot-id]')].map((e) => e.dataset.botId)

test('groupClusterItems：≥2 才成群，單顆與 null 維持單列，群集落在第一個成員的位置', () => {
  const items = [bot('a', { cluster: 'x' }), bot('b'), bot('c', { cluster: 'x' }), bot('d', { cluster: 'y' })]
  const g = groupClusterItems(items)
  assert.deepEqual(g.map((e) => (e.kind === 'cluster' ? `c:${e.name}:${e.members.length}` : `s:${e.item.id}`)), ['c:x:2', 's:b', 's:d'])
})

test('收合：一列群集、計數、成員不畫，單列照舊', async () => {
  seed()
  await mount(<Sidebar />)
  await settle()
  const row = clusterRow()!
  assert.ok(row)
  assert.match(row.textContent ?? '', /haiku/)
  // 狀態與數量合併在標題上：燈號＋數字（1 執行中、2 閒置），完整文字在 aria-label／title，沒有另一行。
  const count = row.querySelector('.bot-cluster-count')!
  assert.equal(count.getAttribute('aria-label'), '共 3 顆：1 執行中 · 2 閒置')
  assert.deepEqual([...count.querySelectorAll('.bot-cluster-stat-item')].map((e) => `${e.querySelector('.lamp')!.className.match(/lamp-(\w+)/)![1]}:${e.textContent}`), ['working:1', 'idle:2'])
  assert.doesNotMatch(row.textContent ?? '', /執行中|閒置|×/)
  assert.ok(!rowIds().includes('h1'))
  assert.ok(rowIds().includes('solo'))
})

test('收合列也顯示 kind 圖示、身份、模型與強度（取第一顆成員）', async () => {
  seed()
  useStore.setState((st) => ({
    bots: st.bots.map((b) => (b.cluster === 'haiku' ? { ...b, identity: 'cc1', effort: 'medium' } : b)),
    runs: {
      ...st.runs,
      h1: { ...st.runs.h1, runtime_model: 'claude-haiku-5-5', runtime_effort: 'medium', runtime_fast: null, runtime_identity: 'cc1' },
    },
  }) as never)
  await mount(<Sidebar />)
  await settle()
  const row = clusterRow()!
  assert.ok(row.querySelector('.kind-tag.claude'), 'kind 圖示')
  const identity = row.querySelector('.identity-badge')
  assert.ok(identity && /cc1/.test(identity.textContent ?? ''), '身份 cc1')
  const model = row.querySelector('.model-tag')
  assert.ok(model && /haiku/.test(model.textContent ?? ''), '模型 haiku')
  assert.ok(model && /Medium/.test(model.textContent ?? ''), '強度 Medium')
  assert.equal(row.querySelector('.bot-cluster-count')!.getAttribute('title'), '共 3 顆：1 執行中 · 2 閒置')
})

test('展開：顯示成員且各自有勾選框，狀態記在 localStorage', async () => {
  seed()
  await mount(<Sidebar />)
  await settle()
  await click(clusterRow()!.querySelector('.bot-cluster-main')!)
  await settle()
  assert.deepEqual(['h1', 'h2', 'h3'].filter((id) => rowIds().includes(id)), ['h1', 'h2', 'h3'])
  assert.equal(document.querySelectorAll('.bot-cluster-members .bot-manager-use-checkbox').length, 3)
  assert.equal(window.localStorage.getItem('agm.expertCluster.p1.haiku'), '1')
})

const chip = () => clusterRow()!.querySelector<HTMLButtonElement>('.bot-cluster-chip')!
const menuItems = () => [...document.querySelectorAll<HTMLButtonElement>('.bot-cluster-chip-menu [role="menuitem"]')]
const patchesOf = () => requests.filter((r) => r.method === 'PATCH')

test('群集列沒有整組勾選框，左欄留空（跟成員列同寬）', async () => {
  seed()
  await mount(<Sidebar />)
  await settle()
  const row = clusterRow()!
  assert.equal(row.querySelector('.bot-manager-use-checkbox'), null)
  assert.ok(row.firstElementChild?.classList.contains('bot-cluster-gutter'))
})

test('全可用：晶片「可派 3/3」，全部可派時標 is-all-on（桌面只在 hover 顯示）', async () => {
  seed([true, true, true])
  await mount(<Sidebar />)
  await settle()
  assert.equal(chip().textContent, '可派 3/3')
  assert.ok(chip().classList.contains('is-all-on'))
})

test('混合：晶片「停派 1/3」，不 hover 也在 DOM 且沒有 is-all-on', async () => {
  seed([true, false, true])
  await mount(<Sidebar />)
  await settle()
  assert.equal(chip().textContent, '停派 1/3')
  assert.ok(!chip().classList.contains('is-all-on'))
})

test('全停派：晶片「停派 3/3」', async () => {
  seed([false, false, false])
  await mount(<Sidebar />)
  await settle()
  assert.equal(chip().textContent, '停派 3/3')
})

test('點晶片只開選單：不送 PATCH、不展開群集；Esc 關閉', async () => {
  seed([true, false, true])
  await mount(<Sidebar />)
  await settle()
  await click(chip())
  await settle()
  assert.equal(chip().getAttribute('aria-expanded'), 'true')
  assert.equal(menuItems().length, 2)
  assert.equal(patchesOf().length, 0)
  assert.equal(clusterRow()!.querySelector('.bot-cluster-main')!.getAttribute('aria-expanded'), 'false')
  chip().dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  await settle()
  assert.equal(document.querySelector('.bot-cluster-chip-menu'), null)
})

test('混合時「全部允許派工」只 PATCH 停派那幾顆（manager_use=true）', async () => {
  seed([true, false, false])
  await mount(<Sidebar />)
  await settle()
  await click(chip())
  await settle()
  await click(menuItems()[0])
  await settle()
  const patches = patchesOf()
  assert.deepEqual(new Set(patches.map((r) => r.path.split('/').pop())), new Set(['h2', 'h3']))
  assert.ok(patches.every((r) => (r.body as { manager_use?: boolean }).manager_use === true))
})

test('混合時「全部停派」只 PATCH 可派那幾顆（manager_use=false）', async () => {
  seed([true, false, true])
  await mount(<Sidebar />)
  await settle()
  await click(chip())
  await settle()
  await click(menuItems()[1])
  await settle()
  const patches = patchesOf()
  assert.deepEqual(new Set(patches.map((r) => r.path.split('/').pop())), new Set(['h1', 'h3']))
  assert.ok(patches.every((r) => (r.body as { manager_use?: boolean }).manager_use === false))
})

test('全可用時「全部允許派工」disabled', async () => {
  seed([true, true, true])
  await mount(<Sidebar />)
  await settle()
  await click(chip())
  await settle()
  assert.equal(menuItems()[0].disabled, true)
  assert.equal(menuItems()[1].disabled, false)
})

test('全停派時「全部停派」disabled', async () => {
  seed([false, false, false])
  await mount(<Sidebar />)
  await settle()
  await click(chip())
  await settle()
  assert.equal(menuItems()[0].disabled, false)
  assert.equal(menuItems()[1].disabled, true)
})

test('一顆 PATCH 失敗：跳錯誤「2 顆已更新，1 顆沒成功」；全成功不跳', async () => {
  seed([false, false, false])
  requests = fakeApi((req) => {
    if (req.method !== 'PATCH') return undefined
    if (req.path.endsWith('/h2')) throw new Error('boom')
    return { needs_restart: false }
  })
  await mount(<Sidebar />)
  await settle()
  await click(chip())
  await settle()
  await click(menuItems()[0])
  await settle()
  await settle()
  const notes = useStore.getState().notices ?? []
  assert.ok(notes.some((n: { text?: string }) => n.text === '2 顆已更新，1 顆沒成功'), JSON.stringify(notes))
})

test('全成功不跳錯誤通知', async () => {
  seed([false, false, false])
  await mount(<Sidebar />)
  await settle()
  await click(chip())
  await settle()
  await click(menuItems()[0])
  await settle()
  await settle()
  const notes = useStore.getState().notices ?? []
  assert.ok(!notes.some((n: { text?: string }) => /沒成功/.test(n.text ?? '')))
})

test('patch 進行中晶片 disabled', async () => {
  seed([true, true, true])
  useStore.setState({ busy: { 'patch:h1': true } } as never)
  await mount(<Sidebar />)
  await settle()
  assert.equal(chip().disabled, true)
})
