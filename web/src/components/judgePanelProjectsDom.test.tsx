/** Jev 全域專案選項在設定表單中的勾選與停用狀態（issue #1509）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import type { Project } from '../api/types'
import { mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { JudgePanel } from './JudgePanel'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

test('設定含 * 時，全域列已勾選、逐一專案也顯示已勾並停用', async () => {
  resetStoreForTest()
  useStore.setState({
    projects: [
      { id: 'P1', label: '第一個專案' } as Project,
      { id: 'P2', label: '未來專案' } as Project,
    ],
  })
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (path === '/judge/settings') {
      return new Response(JSON.stringify({ enabled: true, projects: ['*'], model: 'jev-1.13.0', key_present: true, key_error: null }), { status: 200 })
    }
    if (path === '/judge/shadow') return new Response(JSON.stringify({ enabled: true, projects: ['*'], model: 'jev-1.13.0', rows: [] }), { status: 200 })
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch

  const root = await mount(<JudgePanel />)
  await settle()

  const all = root.querySelector<HTMLInputElement>('.judge-all-projects input')
  assert.ok(all, '專案區要有全部專案開關')
  assert.equal(all.checked, true)
  const individual = [...root.querySelectorAll<HTMLInputElement>('.judge-projects label:not(.judge-all-projects) input')]
  assert.equal(individual.length, 2)
  assert.ok(individual.every((input) => input.checked && input.disabled), '全域開啟時每個專案應顯示已勾且 disabled')
  assert.match(root.querySelector('.judge-all-projects')?.textContent ?? '', /所有專案的對應內容都會送到 TypeSafe/)
})
