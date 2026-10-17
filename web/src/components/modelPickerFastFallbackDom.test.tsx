/**
 * 模型 API 失敗時（退回內建清單）換模型，不可以因為內建清單「不知道」能力就把 Fast 關掉（issue #1810）。
 * 只有清單來自 API、而且新模型明確沒有 priority tier，才自動關 Fast（同 `modelSwitchPatch` 的 fromApi 規則）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { ModelInfo } from '../api/types'
import { FAST_TIER, MODEL_OPTIONS } from '../api/types'
import { ApiModelFields } from './ModelPicker'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

function info(id: string, fast: boolean): ModelInfo {
  return {
    id,
    display_name: id,
    description: '',
    is_default: false,
    default_effort: null,
    efforts: [],
    service_tiers: fast ? [{ id: FAST_TIER, name: 'Fast', description: '' }] : [],
  }
}

function modelButtons(el: HTMLElement): HTMLButtonElement[] {
  return [...el.querySelectorAll<HTMLButtonElement>('.opt-group.models .opt')]
}

test('API 失敗走內建清單：Fast 開著、切到另一個模型，不呼叫 onFast（不清掉使用者的設定）', async () => {
  await act(() => useStore.setState({ models: { 'codex@local@': null } } as never))
  const fastChanges: boolean[] = []
  const [first, second] = MODEL_OPTIONS.codex
  const el = await mount(
    <ApiModelFields kind="codex" host="local" model={first} onModel={() => {}} effort={null} onEffort={() => {}} fast onFast={(v) => fastChanges.push(v)} />,
  )
  const other = modelButtons(el).find((b) => b.title.startsWith(second))
  assert.ok(other, '內建清單裡有另一個模型')
  await click(other)
  assert.deepEqual(fastChanges, [], '內建清單沒有能力資料，不該替使用者關 Fast')
})

test('API 正常、新模型明確沒有 Fast tier：切過去時 onFast(false)', async () => {
  const api = [info('fast-model', true), info('plain-model', false)]
  await act(() => useStore.setState({ models: { 'codex@local@': api } } as never))
  const fastChanges: boolean[] = []
  const el = await mount(
    <ApiModelFields kind="codex" host="local" model="fast-model" onModel={() => {}} effort={null} onEffort={() => {}} fast onFast={(v) => fastChanges.push(v)} />,
  )
  const plain = modelButtons(el).find((b) => b.title.startsWith('plain-model'))
  assert.ok(plain, 'API 清單裡有 plain-model')
  await click(plain)
  assert.deepEqual(fastChanges, [false])
})

test('API 正常、新模型有 Fast tier：切過去時不動 Fast', async () => {
  const api = [info('fast-model', true), info('other-fast', true)]
  await act(() => useStore.setState({ models: { 'codex@local@': api } } as never))
  const fastChanges: boolean[] = []
  const el = await mount(
    <ApiModelFields kind="codex" host="local" model="fast-model" onModel={() => {}} effort={null} onEffort={() => {}} fast onFast={(v) => fastChanges.push(v)} />,
  )
  const other = modelButtons(el).find((b) => b.title.startsWith('other-fast'))
  assert.ok(other, 'API 清單裡有 other-fast')
  await click(other)
  assert.deepEqual(fastChanges, [])
})
