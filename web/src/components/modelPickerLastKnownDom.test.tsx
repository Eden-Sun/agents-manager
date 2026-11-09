/** 模型清單載入中／API 失敗時，先用最後抓到的清單，不退回寫死的（使用者 2026-11-09）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { saveLastModels } from '../store/modelsCache'
import { ApiModelFields } from './ModelPicker'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const info = (id: string, i: number) => ({ id, display_name: id, description: '', is_default: i === 0, default_effort: null, efforts: ['low', 'high'], service_tiers: [] })

function fields() {
  return mount(<ApiModelFields kind="codex" host="local" model={null} onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />)
}
const titles = (el: Element) => [...el.querySelectorAll('.opt-group.models .opt')].map((b) => b.getAttribute('title') ?? '')

test('API 失敗：顯示上次取得的清單（含 6.1 型號）與對應提示', async () => {
  saveLastModels('codex@local@', [info('gpt-6.1-luna', 0), info('gpt-6.1-sol', 1)])
  await act(() => useStore.setState({ models: { 'codex@local@': null }, modelsFailedAt: { 'codex@local@': Date.now() } } as never))
  const el = await fields()
  assert.ok(titles(el).some((t) => t.includes('gpt-6.1-sol')), titles(el).join('|'))
  assert.match(el.querySelector('.field-note.warn')?.textContent ?? '', /上次取得|last list/)
})

test('載入中：先顯示上次取得的清單並標載入中', async () => {
  saveLastModels('codex@local@', [info('gpt-6.1-luna', 0)])
  await act(() => useStore.setState({ models: {}, modelsFailedAt: {}, loadModels: () => new Promise(() => {}) } as never))
  const el = await fields()
  assert.ok(titles(el).some((t) => t.includes('gpt-6.1-luna')))
  assert.match(el.querySelector('.field-note')?.textContent ?? '', /載入中|Loading/)
})
