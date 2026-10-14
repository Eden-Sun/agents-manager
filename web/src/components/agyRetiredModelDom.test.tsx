/**
 * agy 只收 Gemini 3.8 含以上（2026-10-14；2026-10-05 起是只留 3.8 Flash 三檔）：不收的模型不進選單；已存在的 bot 設了它們，設定面板講清楚「啟動時改用 3.8 medium」並給一鍵改掉。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { ModelInfo } from '../api/types'
import { MODEL_OPTIONS, canonicalModel } from '../api/types'
import { ApiModelFields } from './ModelPicker'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const RETIRED = ['gemini-3.7-flash-high', 'gemini-3.6-flash-low', 'gemini-3.1-pro-high', 'claude-sonnet-4-6', 'claude-opus-4-6-thinking', 'gpt-oss-120b-medium']

async function picker(model: string | null, onModel: (v: string | null) => void = () => {}) {
  // 不打 API：快取放一份 null（上次失敗），退回內建清單。
  await act(() => useStore.setState({ models: { 'agy@local@': null }, modelsFailedAt: { 'agy@local@': Date.now() } } as never))
  return mount(<ApiModelFields kind="agy" host="local" model={model} onModel={onModel} effort={null} onEffort={() => {}} fast={false} onFast={() => {}} />)
}

test('後備清單（API 不可用）是 3.8 Flash 三檔，不收的模型一顆按鈕都沒有', async () => {
  const el = await picker('gemini-3.8-flash-high')
  const buttons = [...el.querySelectorAll('.opt-group.models .opt')].map((b) => b.getAttribute('title') ?? '')
  assert.equal(buttons.length, 3)
  for (const id of MODEL_OPTIONS.agy) assert.ok(buttons.some((t) => t.startsWith(id)), id)
  for (const retired of RETIRED) assert.ok(!buttons.some((t) => t.includes(retired)), retired)
  assert.ok(buttons[0].includes('模型預設'), '預設維持 3.8 medium')
  assert.equal(el.querySelector('.field-note.warn.hint, .hint.field-note.warn'), null, '3.8 沒有下架提示')
})

function info(id: string, is_default = false): ModelInfo {
  return { id, display_name: id, description: '', is_default, default_effort: null, efforts: [], service_tiers: [] }
}

test('API 給的清單照規則過濾：3.8 含以上留下、3.7／3.6／3.1 Pro／claude／gpt-oss 不出現', async () => {
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
  const buttons = [...el.querySelectorAll('.opt-group.models .opt')].map((b) => b.getAttribute('title') ?? '')
  assert.deepEqual(
    buttons.map((t) => t.split(' ')[0]),
    ['gemini-3.8-flash-medium', 'gemini-3.8-flash-high', 'gemini-4-argon'],
    '預設排第一，其餘照 API 順序',
  )
  assert.ok(buttons[0].includes('模型預設'))
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
