import test from 'node:test'
import assert from 'node:assert/strict'
import type { ModelInfo } from '../api/types.ts'
import { compareVersion, latestModels, parseModelSeries } from './modelSeries.ts'

const m = (id: string): ModelInfo => ({
  id,
  display_name: id,
  description: '',
  is_default: false,
  default_effort: null,
  efforts: [],
  service_tiers: [],
})
const ids = (xs: ModelInfo[]) => xs.map((x) => x.id)

test('版號逐段數字比較：6.10 > 6.2，缺段當 0', () => {
  assert.ok(compareVersion([6, 10], [6, 2]) > 0)
  assert.ok(compareVersion([6, 2], [6, 10]) < 0)
  assert.equal(compareVersion([6], [6, 0]), 0)
  assert.ok(compareVersion([], [5, 5]) < 0)
})

test('codex 依 gpt-<版號>-<系列> 拆；拆不出的回 null', () => {
  assert.deepEqual(parseModelSeries('codex', 'gpt-6.2-sol'), { series: 'codex:sol', version: [6, 2] })
  assert.equal(parseModelSeries('codex', 'alpha'), null)
})

test('codex：每個系列只留版號最大的；6.10 勝過 6.2', () => {
  const r = latestModels('codex', [m('gpt-6-luna'), m('gpt-6-astra'), m('gpt-6.2-sol'), m('gpt-6.2-astra'), m('gpt-6.1-terra'), m('gpt-6.1-sol'), m('gpt-6.10-sol')], null)
  assert.deepEqual(ids(r.models), ['gpt-6-luna', 'gpt-6.2-astra', 'gpt-6.1-terra', 'gpt-6.10-sol'])
  assert.equal(r.oldIds.size, 0)
})

test('無法判斷系列的模型照舊保留（不被隱藏）', () => {
  const r = latestModels('codex', [m('alpha'), m('beta'), m('gpt-6-sol'), m('gpt-6.1-sol')], null)
  assert.deepEqual(ids(r.models), ['alpha', 'beta', 'gpt-6.1-sol'])
})

test('被隱藏的舊版若是目前設定的模型：保留並標為舊版', () => {
  const r = latestModels('codex', [m('gpt-6.1-sol'), m('gpt-6-sol'), m('gpt-6-luna')], 'gpt-6-sol')
  assert.deepEqual(ids(r.models), ['gpt-6.1-sol', 'gpt-6-sol', 'gpt-6-luna'])
  assert.deepEqual([...r.oldIds], ['gpt-6-sol'])
  const none = latestModels('codex', [m('gpt-6.1-sol'), m('gpt-6-sol')], 'gpt-6-luna')
  assert.deepEqual(ids(none.models), ['gpt-6.1-sol'])
})

test('claude：完整版號勝過別名；別名只在沒有完整版號時當最新；帶 [1m] 的同版號一起保留', () => {
  const r = latestModels('claude', [m('opus'), m('claude-opus-5-5'), m('claude-opus-4-8'), m('sonnet'), m('claude-opus-5-5[1m]')], null)
  assert.deepEqual(ids(r.models), ['claude-opus-5-5', 'sonnet', 'claude-opus-5-5[1m]'])
  const alias = latestModels('claude', [m('opus'), m('haiku')], 'opus')
  assert.deepEqual(ids(alias.models), ['opus', 'haiku'])
  assert.equal(alias.oldIds.size, 0)
})

test('agy：同版號的強度變體全保留，舊版號隱藏', () => {
  const r = latestModels('agy', [
    m('gemini-3.8-flash-medium'),
    m('gemini-3.8-flash-high'),
    m('gemini-3.8-flash-low'),
    m('gemini-3.7-flash-high'),
    m('gemini-3.8-pro-low'),
  ], null)
  assert.deepEqual(ids(r.models), ['gemini-3.8-flash-medium', 'gemini-3.8-flash-high', 'gemini-3.8-flash-low', 'gemini-3.8-pro-low'])
})

test('grok 等其他 kind 不分組，全部照舊', () => {
  const all = ['grok-4.7', 'grok-4.7-build-fast', 'grok-4.6', 'grok-4.5'].map(m)
  assert.deepEqual(ids(latestModels('grok', all, null).models), ids(all))
})
