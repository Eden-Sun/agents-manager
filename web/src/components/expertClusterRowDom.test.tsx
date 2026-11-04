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

const project: Project = { id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', executor_pool: [], workspace_id: null, github: null, created_at: '' } as unknown as Project

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

test('整組勾選在列的最左，跟成員列同一欄，已勾仍可取消', async () => {
  seed()
  await mount(<Sidebar />)
  await settle()
  const row = clusterRow()!
  const wrap = row.querySelector('.bot-manager-use-wrap')!
  const main = row.querySelector('.bot-cluster-main')!
  assert.equal(row.firstElementChild, wrap, '勾選框是列的第一個子元素')
  assert.ok((wrap.compareDocumentPosition(main) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0)
  const box = wrap.querySelector('input')!
  assert.equal(box.checked, true)
  assert.equal(box.disabled, false, '群集框不鎖，全勾仍可關掉')
  assert.equal(box.classList.contains('is-locked'), false)

  await click(row.querySelector('.bot-cluster-main')!)
  await settle()
  const member = document.querySelector<HTMLElement>('.bot-cluster-members .bot-row')!
  const memberWrap = member.querySelector('.bot-manager-use-wrap')!
  assert.equal(member.firstElementChild, memberWrap, '成員列的勾選框也在最左')
})

test('整組勾選：全勾 → 取消送 3 個 PATCH manager_use=false', async () => {
  seed()
  await mount(<Sidebar />)
  await settle()
  const box = clusterRow()!.querySelector<HTMLInputElement>('.bot-manager-use-checkbox')!
  assert.equal(box.checked, true)
  assert.equal(box.indeterminate, false)
  await click(box)
  await settle()
  const patches = requests.filter((r) => r.method === 'PATCH')
  assert.equal(patches.length, 3)
  assert.deepEqual(new Set(patches.map((r) => r.path.split('/').pop())), new Set(['h1', 'h2', 'h3']))
  assert.ok(patches.every((r) => (r.body as { manager_use?: boolean }).manager_use === false))
})

test('部分勾：indeterminate，點一下全勾（3 個 PATCH manager_use=true）', async () => {
  seed([true, false, false])
  await mount(<Sidebar />)
  await settle()
  const box = clusterRow()!.querySelector<HTMLInputElement>('.bot-manager-use-checkbox')!
  assert.equal(box.indeterminate, true)
  assert.equal(box.checked, false)
  await click(box)
  await settle()
  const patches = requests.filter((r) => r.method === 'PATCH')
  assert.equal(patches.length, 3)
  assert.ok(patches.every((r) => (r.body as { manager_use?: boolean }).manager_use === true))
})
