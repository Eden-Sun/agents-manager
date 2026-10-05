/**
 * agy 只留 Gemini 3.8 Flash（2026-10-05）：選單只有三檔；已存在的 bot 設了拿掉的模型，設定面板講清楚「啟動時改用 3.8 medium」並給一鍵改掉。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
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

test('選單只有 3.8 Flash 三檔，拿掉的模型一顆按鈕都沒有', async () => {
  const el = await picker('gemini-3.8-flash-high')
  const buttons = [...el.querySelectorAll('.opt-group.models .opt')].map((b) => b.getAttribute('title') ?? '')
  assert.equal(buttons.length, 3)
  for (const id of MODEL_OPTIONS.agy) assert.ok(buttons.some((t) => t.startsWith(id)), id)
  for (const retired of RETIRED) assert.ok(!buttons.some((t) => t.includes(retired)), retired)
  assert.ok(buttons[0].includes('模型預設'), '預設維持 3.8 medium')
  assert.equal(el.querySelector('.field-note.warn.hint, .hint.field-note.warn'), null, '3.8 沒有下架提示')
})

test('存著拿掉的模型：設定面板講清楚啟動時改用 3.8 medium，一鍵改掉', async () => {
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
