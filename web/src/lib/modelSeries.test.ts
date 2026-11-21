import test from 'node:test'
import assert from 'node:assert/strict'
import { agyModelAllowed, type ModelInfo } from '../api/types.ts'
import { compareVersion, ensureCurrentModel, latestModels, parseModelSeries } from './modelSeries.ts'
import { compareAgyModels, formatAgyEffortLabel, formatAgyModelLabel, parseAgyModel, sortAgyModelIds } from './agyModel.ts'

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

test('agy：四個家族（flash, hydrogen, argon, neon 包含 xhigh）都保留，家族不同不可互相隱藏', () => {
  const models14 = [
    m('gemini-3.8-flash-low'),
    m('gemini-3.8-flash-medium'),
    m('gemini-3.8-flash-high'),
    m('gemini-4-hydrogen-low'),
    m('gemini-4-hydrogen-medium'),
    m('gemini-4-hydrogen-high'),
    m('gemini-4-hydrogen-xhigh'),
    m('gemini-4-argon-low'),
    m('gemini-4-argon-medium'),
    m('gemini-4-argon-high'),
    m('gemini-4-neon-low'),
    m('gemini-4-neon-medium'),
    m('gemini-4-neon-high'),
    m('gemini-4-neon-xhigh'),
  ]
  const r = latestModels('agy', models14, null)
  assert.equal(r.models.length, 14, '14 個模型全部保留')
  assert.deepEqual(ids(r.models), ids(models14), '四家族模型不互相隱藏')
  assert.equal(r.oldIds.size, 0)
})

test('grok 等其他 kind 不分組，全部照舊', () => {
  const all = ['grok-4.7', 'grok-4.7-build-fast', 'grok-4.6', 'grok-4.5'].map(m)
  assert.deepEqual(ids(latestModels('grok', all, null).models), ids(all))
})

test('parseAgyModel：解析 gemini-<版號>-<家族>[-<強度>]', () => {
  const p1 = parseAgyModel('gemini-3.8-flash-low')
  assert.ok(p1)
  assert.equal(p1?.version, '3.8')
  assert.deepEqual(p1?.versionNumbers, [3, 8])
  assert.equal(p1?.family, 'flash')
  assert.equal(p1?.familyRank, 0)
  assert.equal(p1?.effort, 'low')
  assert.equal(p1?.effortRank, 0)

  const p2 = parseAgyModel('gemini-4-hydrogen-xhigh')
  assert.ok(p2)
  assert.equal(p2?.version, '4')
  assert.deepEqual(p2?.versionNumbers, [4])
  assert.equal(p2?.family, 'hydrogen')
  assert.equal(p2?.familyRank, 1)
  assert.equal(p2?.effort, 'xhigh')
  assert.equal(p2?.effortRank, 3)

  const p3 = parseAgyModel('gemini-4-argon')
  assert.ok(p3)
  assert.equal(p3?.family, 'argon')
  assert.equal(p3?.familyRank, 2)
  assert.equal(p3?.effort, undefined)
  assert.equal(p3?.effortRank, -1)

  const p4 = parseAgyModel('gemini-4-neon-medium')
  assert.ok(p4)
  assert.equal(p4?.family, 'neon')
  assert.equal(p4?.familyRank, 3)
  assert.equal(p4?.effort, 'medium')
  assert.equal(p4?.effortRank, 1)

  const pUnknownFam = parseAgyModel('gemini-4-custom-high')
  assert.ok(pUnknownFam)
  assert.equal(pUnknownFam?.family, 'custom')
  assert.equal(pUnknownFam?.familyRank, 99)

  assert.equal(parseAgyModel('unknown-model'), null)
  assert.equal(parseAgyModel('claude-opus-5-5'), null)
  assert.equal(parseAgyModel('gemini-flash'), null)
})

test('compareAgyModels：家族由便宜到貴：flash < hydrogen < argon < neon', () => {
  assert.ok(compareAgyModels('gemini-3.8-flash-low', 'gemini-4-hydrogen-low') < 0)
  assert.ok(compareAgyModels('gemini-4-hydrogen-low', 'gemini-4-argon-low') < 0)
  assert.ok(compareAgyModels('gemini-4-argon-low', 'gemini-4-neon-low') < 0)
})

test('sortAgyModelIds：14 個模型亂序輸入 → flash(low,medium,high)、hydrogen(…)、argon(…)、neon(…)', () => {
  const input = [
    'gemini-4-neon-high',
    'gemini-4-argon-low',
    'gemini-3.8-flash-high',
    'gemini-4-hydrogen-xhigh',
    'gemini-4-neon-low',
    'gemini-3.8-flash-low',
    'gemini-4-argon-high',
    'gemini-4-hydrogen-medium',
    'gemini-4-neon-medium',
    'gemini-3.8-flash-medium',
    'gemini-4-hydrogen-low',
    'gemini-4-argon-medium',
    'gemini-4-neon-xhigh',
    'gemini-4-hydrogen-high',
  ]
  const expected = [
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
  ]
  const actual = sortAgyModelIds(input)
  assert.deepEqual(actual, expected)
})

test('sortAgyModelIds：未知家族排最後、照原順序；同未知家族依版本由舊到新、再依強度', () => {
  const input = [
    'gemini-4-customb-high',
    'gemini-3.8-flash-medium',
    'gemini-5-customa-medium',
    'gemini-4-customa-high',
    'gemini-4-customa-low',
    'gemini-4-argon-low',
  ]
  const expected = [
    'gemini-3.8-flash-medium',
    'gemini-4-argon-low',
    'gemini-4-customb-high',
    'gemini-4-customa-low',
    'gemini-4-customa-high',
    'gemini-5-customa-medium',
  ]
  const actual = sortAgyModelIds(input)
  assert.deepEqual(actual, expected)
})

test('sortAgyModelIds：認不得的 id 排最後原樣', () => {
  const input = [
    'invalid-model-1',
    'gemini-4-argon-low',
    'another-bad-id',
    'gemini-3.8-flash-high',
    'custom-raw-string',
  ]
  const expected = [
    'gemini-3.8-flash-high',
    'gemini-4-argon-low',
    'invalid-model-1',
    'another-bad-id',
    'custom-raw-string',
  ]
  const actual = sortAgyModelIds(input)
  assert.deepEqual(actual, expected)
})

test('formatAgyModelLabel：由 id 推導「<版號>-<家族首字大寫> <強度>」，認不得的保留原樣', () => {
  assert.equal(formatAgyEffortLabel('low'), 'Low')
  assert.equal(formatAgyEffortLabel('medium'), 'Medium')
  assert.equal(formatAgyEffortLabel('high'), 'High')
  assert.equal(formatAgyEffortLabel('xhigh'), 'XHigh')
  assert.equal(formatAgyModelLabel('gemini-3.8-flash-low'), '3.8-Flash Low')
  assert.equal(formatAgyModelLabel('gemini-3.8-flash-medium'), '3.8-Flash Medium')
  assert.equal(formatAgyModelLabel('gemini-3.8-flash-high'), '3.8-Flash High')
  assert.equal(formatAgyModelLabel('gemini-4-hydrogen-low'), '4-Hydrogen Low')
  assert.equal(formatAgyModelLabel('gemini-4-hydrogen-xhigh'), '4-Hydrogen XHigh')
  assert.equal(formatAgyModelLabel('gemini-4-argon'), '4-Argon')
  assert.equal(formatAgyModelLabel('gemini-4-argon-medium'), '4-Argon Medium')
  assert.equal(formatAgyModelLabel('gemini-4-neon-medium'), '4-Neon Medium')
  assert.equal(formatAgyModelLabel('gemini-4-neon-xhigh'), '4-Neon XHigh')
  assert.equal(formatAgyModelLabel('unknown-model'), 'unknown-model')
  assert.equal(formatAgyModelLabel('gpt-6-luna'), 'gpt-6-luna')
})

test('ensureCurrentModel：目前模型不在清單時補上虛擬項目並標記 missingIds', () => {
  const staticList = [m('gemini-3.8-flash-low'), m('gemini-3.8-flash-medium'), m('gemini-3.8-flash-high')]
  const res = ensureCurrentModel(staticList, 'gemini-4-neon-medium', 'agy')
  assert.equal(res.models.length, 4)
  assert.equal(res.missingIds.has('gemini-4-neon-medium'), true)
  assert.ok(res.models.some((x) => x.id === 'gemini-4-neon-medium'))

  const resExists = ensureCurrentModel(staticList, 'gemini-3.8-flash-medium', 'agy')
  assert.equal(resExists.models.length, 3)
  assert.equal(resExists.missingIds.size, 0)

  const resRetired = ensureCurrentModel(staticList, 'gemini-3.1-pro-high', 'agy')
  assert.equal(resRetired.models.length, 3)
  assert.equal(resRetired.missingIds.size, 0)
})

test('Orchestrator：標籤「Orchestrator Neon」「Orchestrator Argon」（g1-pro 即 Argon）', () => {
  assert.equal(formatAgyModelLabel('gemini-pro-orchestrator-neon'), 'Orchestrator Neon')
  assert.equal(formatAgyModelLabel('gemini-pro-orchestrator-g1-pro'), 'Orchestrator Argon')
})

test('sortAgyModelIds：Orchestrator 排在 Gemini 4 之後、認不得的之前，組內保持原順序', () => {
  const input = ['unknown-x', 'gemini-pro-orchestrator-neon', 'gemini-4-neon-high', 'gemini-3.8-flash-low', 'gemini-pro-orchestrator-g1-pro']
  assert.deepEqual(sortAgyModelIds(input), [
    'gemini-3.8-flash-low',
    'gemini-4-neon-high',
    'gemini-pro-orchestrator-neon',
    'gemini-pro-orchestrator-g1-pro',
    'unknown-x',
  ])
})

test('agyModelAllowed：Orchestrator slug 放行；第三方與 3.7 以下仍擋', () => {
  assert.equal(agyModelAllowed('gemini-pro-orchestrator-neon'), true)
  assert.equal(agyModelAllowed('gemini-pro-orchestrator-g1-pro'), true)
  for (const no of ['gemini-pro-orchestrator-', 'gemini-pro-orchestrator', 'gemini-pro-orchestrator-Neon', 'claude-opus-5-5-high', 'claude-sonnet-5-5-medium', 'gpt-oss-120b-medium', 'gemini-3.7-flash-high']) {
    assert.equal(agyModelAllowed(no), false, no)
  }
})
