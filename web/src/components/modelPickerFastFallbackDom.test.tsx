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

test('API 失敗走內建清單：切模型時不動 effort（不把使用者設定的 effort 清成 null）(#1821)', async () => {
  await act(() => useStore.setState({ models: { 'codex@local@': null } } as never))
  const effortChanges: (string | null)[] = []
  const [first, second] = MODEL_OPTIONS.codex
  const el = await mount(
    <ApiModelFields kind="codex" host="local" model={first} onModel={() => {}} effort="high" onEffort={(v) => effortChanges.push(v)} fast={false} onFast={() => {}} />,
  )
  const other = modelButtons(el).find((b) => b.title.startsWith(second))
  assert.ok(other, '內建清單裡有另一個模型')
  await click(other)
  assert.deepEqual(effortChanges, [], '內建清單 efforts 為空時不動 effort')
})

test('API 清單 efforts 非空且不含目前 effort：改回 default_effort (#1821)', async () => {
  const api: ModelInfo[] = [
    { ...info('model-a', false), efforts: ['low', 'medium', 'high'], default_effort: 'medium' },
    { ...info('model-b', false), efforts: ['low', 'medium'], default_effort: 'low' },
  ]
  await act(() => useStore.setState({ models: { 'codex@local@': api } } as never))
  const effortChanges: (string | null)[] = []
  const el = await mount(
    <ApiModelFields kind="codex" host="local" model="model-a" onModel={() => {}} effort="high" onEffort={(v) => effortChanges.push(v)} fast={false} onFast={() => {}} />,
  )
  const bBtn = modelButtons(el).find((b) => b.title.startsWith('model-b'))
  assert.ok(bBtn, 'API 清單裡有 model-b')
  await click(bBtn)
  assert.deepEqual(effortChanges, ['low'], '不支援目前 effort 時改回新模型的 default_effort')
})

test('API 清單 efforts 非空且包含目前 effort：不動 effort (#1821)', async () => {
  const api: ModelInfo[] = [
    { ...info('model-a', false), efforts: ['low', 'medium', 'high'], default_effort: 'medium' },
    { ...info('model-b', false), efforts: ['low', 'medium', 'high'], default_effort: 'low' },
  ]
  await act(() => useStore.setState({ models: { 'codex@local@': api } } as never))
  const effortChanges: (string | null)[] = []
  const el = await mount(
    <ApiModelFields kind="codex" host="local" model="model-a" onModel={() => {}} effort="high" onEffort={(v) => effortChanges.push(v)} fast={false} onFast={() => {}} />,
  )
  const bBtn = modelButtons(el).find((b) => b.title.startsWith('model-b'))
  assert.ok(bBtn, 'API 清單裡有 model-b')
  await click(bBtn)
  assert.deepEqual(effortChanges, [], '新模型包含目前 effort 時不動 effort')
})

