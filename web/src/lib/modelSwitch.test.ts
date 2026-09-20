import test from 'node:test'
import assert from 'node:assert/strict'
import type { ModelInfo } from '../api/types.ts'
import { modelSwitchPatch } from './modelSwitch.ts'

const m = (id: string, extra: Partial<ModelInfo> = {}): ModelInfo => ({
  id,
  display_name: id,
  description: '',
  is_default: false,
  default_effort: 'medium',
  efforts: ['low', 'medium', 'high'],
  service_tiers: [],
  ...extra,
})
const fastTier = [{ id: 'priority', name: 'Fast', description: '' }]

test('換到沒有 fast tier 的模型：fast 一併關掉', () => {
  const models = [m('a', { service_tiers: fastTier }), m('b')]
  assert.deepEqual(modelSwitchPatch({ effort: 'high', fast: true }, models, 'b', true), { model: 'b', fast: false })
})

test('新模型也有 fast tier：fast 不動', () => {
  const models = [m('a', { service_tiers: fastTier }), m('c', { service_tiers: fastTier })]
  assert.deepEqual(modelSwitchPatch({ effort: 'high', fast: true }, models, 'c', true), { model: 'c' })
})

test('清單是靜態退路（沒有 tier 資訊）：不亂清 fast', () => {
  assert.deepEqual(modelSwitchPatch({ effort: null, fast: true }, [m('b')], 'b', false), { model: 'b' })
})

test('強度不適用新模型：換成新模型的預設（原行為不變）', () => {
  const models = [m('b', { efforts: ['low', 'medium'], default_effort: 'low' })]
  assert.deepEqual(modelSwitchPatch({ effort: 'high', fast: false }, models, 'b', true), { model: 'b', effort: 'low' })
})
