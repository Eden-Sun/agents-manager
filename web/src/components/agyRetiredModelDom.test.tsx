import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { ModelInfo } from '../api/types'
import { canonicalModel } from '../api/types'
import { ApiModelFields } from './ModelPicker'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const RETIRED = ['gemini-3.7-flash-high', 'gemini-3.6-flash-low', 'gemini-3.1-pro-high', 'claude-sonnet-4-6', 'claude-opus-4-6-thinking', 'gpt-oss-120b-medium']

async function picker(model: string | null, onModel: (v: string | null) => void = () => {}) {
  await act(() => useStore.setState({ models: { 'agy@local@': null }, modelsFailedAt: { 'agy@local@': Date.now() } } as never))
  return mount(<ApiModelFields kind="agy" host="local" model={model} onModel={onModel} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />)
}

test('後備清單（API 不可用）：模型列只有 3.8 Flash 一顆，強度列是 Low／Medium／High，不收的模型一顆都沒有', async () => {
  const el = await picker('gemini-3.8-flash-high')
  const models = [...el.querySelectorAll('.opt-group.models .opt')]
  assert.deepEqual(models.map((b) => b.textContent), ['3.8-Flash'])
  assert.equal(models[0].getAttribute('aria-checked'), 'true')
  const efforts = [...el.querySelectorAll('.opt-group:not(.models) .opt')]
  assert.deepEqual(efforts.map((b) => b.textContent), ['Low', 'Medium', 'High'], '由便宜到貴：low < medium < high')
  assert.equal(efforts[2].getAttribute('aria-checked'), 'true', '目前是 high')
  const html = el.innerHTML
  for (const retired of RETIRED) assert.ok(!html.includes(retired), retired)
  assert.equal(el.querySelector('.field-note.warn.hint, .hint.field-note.warn'), null, '3.8 沒有下架提示')
})

function info(id: string, is_default = false): ModelInfo {
  return { id, display_name: id, description: '', is_default, default_effort: null, efforts: [], service_tiers: [] }
}

test('API 給的清單照規則過濾：3.8 含以上留下、3.7／3.6／3.1 Pro／claude／gpt-oss 不出現；3.8 組的強度列只列 Medium／High', async () => {
  const api = [
    info('gemini-3.7-flash-high'),
    info('claude-sonnet-4-6'),
    info('gemini-3.8-flash-high'),
    info('gpt-oss-120b-medium'),
    info('gemini-3.8-flash-medium', true),
    info('gemini-4-argon'),
  ]
  await act(() => useStore.setState({ models: { 'agy@local@': api } } as never))
  const el = await mount(<ApiModelFields kind="agy" host="local" model="gemini-3.8-flash-medium" onModel={() => {}} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />)
  const models = [...el.querySelectorAll('.opt-group.models .opt')]
  assert.deepEqual(models.map((b) => b.textContent), ['3.8-Flash', '4-Argon'], '預設 3.8 組排第一，其次 4-Argon')
  assert.ok(models[0].getAttribute('title')?.includes('模型預設'))
  const efforts = [...el.querySelectorAll('.opt-group:not(.models) .opt')]
  assert.deepEqual(efforts.map((b) => b.textContent), ['Medium', 'High'])
  assert.equal(efforts[0].getAttribute('aria-checked'), 'true')
})

test('存著不收的模型：設定面板講清楚啟動時改用 3.8 medium，一鍵改掉', async () => {
  for (const retired of RETIRED) assert.equal(canonicalModel('agy', retired), 'gemini-3.8-flash-medium', retired)
  let picked: string | null | undefined
  const el = await picker('gemini-3.1-pro-high', (v) => {
    picked = v
  })
  const note = el.querySelector('.hint.field-note.warn')
  assert.ok(note, '看得出來')
  assert.match(note!.textContent ?? '', /gemini-3\.1-pro-high.*不提供.*gemini-3\.8-flash-medium/)
  await click(note!.querySelector('button')!)
  assert.equal(picked, 'gemini-3.8-flash-medium')
})

test('沒設模型（用預設）不出提示', async () => {
  const el = await picker(null)
  assert.equal(el.querySelector('.hint.field-note.warn'), null)
})
