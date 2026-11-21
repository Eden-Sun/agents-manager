import test from 'node:test'
import assert from 'node:assert/strict'
import type { ModelInfo } from '../api/types.ts'
import { agyEffortOf, agyGroupOf, groupAgyModels, pickAgyVariant } from './agyModelGroup.ts'
import { sortAgyModelIds } from './agyModel.ts'

const m = (id: string, extra: Partial<ModelInfo> = {}): ModelInfo => ({
  id,
  display_name: id,
  description: '',
  is_default: false,
  default_effort: null,
  efforts: [],
  service_tiers: [],
  ...extra,
})

/** 16 顆完整清單（Gemini 3.8／4 組合＋兩顆 Orchestrator）。 */
const FULL = [
  'gemini-3.8-flash-low',
  'gemini-3.8-flash-medium',
  'gemini-3.8-flash-high',
  'gemini-4-hydrogen-low',
  'gemini-4-hydrogen-medium',
  'gemini-4-hydrogen-high',
  'gemini-4-hydrogen-xhigh',
  'gemini-4-argon-low',
  'gemini-4-argon-medium',
  'gemini-4-argon-high',
  'gemini-4-neon-low',
  'gemini-4-neon-medium',
  'gemini-4-neon-high',
  'gemini-4-neon-xhigh',
  'gemini-pro-orchestrator-neon',
  'gemini-pro-orchestrator-g1-pro',
]

test('16 個 id 分成 6 組，模型列順序與標籤正確', () => {
  const models = sortAgyModelIds(FULL).map((id) => m(id, { is_default: id === 'gemini-3.8-flash-medium' }))
  const groups = groupAgyModels(models)
  assert.deepEqual(groups.map((g) => g.label), ['3.8-Flash', '4-Hydrogen', '4-Argon', '4-Neon', 'Orchestrator Neon', 'Orchestrator Argon'])
  assert.deepEqual(groups.map((g) => g.efforts), [
    ['low', 'medium', 'high'],
    ['low', 'medium', 'high', 'xhigh'],
    ['low', 'medium', 'high'],
    ['low', 'medium', 'high', 'xhigh'],
    [],
    [],
  ])
  assert.equal(groups[0].isDefault, true, '3.8-Flash 含 CLI 預設')
  assert.equal(groups.filter((g) => g.isDefault).length, 1)
})

test('Orchestrator 與無強度 slug 不畫強度列；未知格式照原樣成組', () => {
  const groups = groupAgyModels([m('gemini-pro-orchestrator-neon'), m('gemini-4-argon'), m('weird-model', { display_name: 'Weird' })])
  assert.equal(groups.length, 3)
  for (const g of groups) assert.deepEqual(g.efforts, [])
  assert.equal(groups[0].label, 'Orchestrator Neon')
  assert.equal(groups[1].label, '4-Argon')
  assert.equal(groups[2].label, 'Weird')
  assert.equal(groups[2].single?.id, 'weird-model')
})

test('強度保留：xhigh → 4-Neon 仍 xhigh；xhigh → 4-Argon（沒有）降為 medium', () => {
  const models = sortAgyModelIds(FULL).map((id) => m(id))
  const groups = groupAgyModels(models)
  const neon = groups.find((g) => g.label === '4-Neon')!
  const argon = groups.find((g) => g.label === '4-Argon')!
  assert.equal(pickAgyVariant(neon, 'xhigh')?.id, 'gemini-4-neon-xhigh')
  assert.equal(pickAgyVariant(argon, 'xhigh')?.id, 'gemini-4-argon-medium')
})

test('沒有 medium 時取第一個（排序後）；沒有目前強度時用 medium', () => {
  const groups = groupAgyModels([m('gemini-4-neon-high'), m('gemini-4-neon-xhigh')])
  assert.equal(pickAgyVariant(groups[0], 'low')?.id, 'gemini-4-neon-high', '沒有 medium → 第一個')
  assert.equal(pickAgyVariant(groups[0], null)?.id, 'gemini-4-neon-high')
})

test('Orchestrator 切換時回唯一的那顆', () => {
  const groups = groupAgyModels([m('gemini-pro-orchestrator-neon')])
  assert.equal(pickAgyVariant(groups[0], 'high')?.id, 'gemini-pro-orchestrator-neon')
})

test('agyGroupOf／agyEffortOf 反查完整 slug 的組與強度', () => {
  const groups = groupAgyModels(sortAgyModelIds(FULL).map((id) => m(id)))
  assert.equal(agyGroupOf(groups, 'gemini-4-hydrogen-xhigh')?.label, '4-Hydrogen')
  assert.equal(agyEffortOf(agyGroupOf(groups, 'gemini-4-hydrogen-xhigh'), 'gemini-4-hydrogen-xhigh'), 'xhigh')
  assert.equal(agyGroupOf(groups, 'gemini-pro-orchestrator-neon')?.label, 'Orchestrator Neon')
  assert.equal(agyEffortOf(agyGroupOf(groups, 'gemini-pro-orchestrator-neon'), 'gemini-pro-orchestrator-neon'), null)
  assert.equal(agyGroupOf(groups, 'gemini-9-nope'), null)
})

test('ag1 只有 3.8 Flash 三檔：一組、三個強度', () => {
  const groups = groupAgyModels(['gemini-3.8-flash-low', 'gemini-3.8-flash-medium', 'gemini-3.8-flash-high'].map((id) => m(id)))
  assert.equal(groups.length, 1)
  assert.deepEqual(groups[0].efforts, ['low', 'medium', 'high'])
})
