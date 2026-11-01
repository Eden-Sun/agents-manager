import test from 'node:test'
import assert from 'node:assert/strict'
import type { ModelInfo } from '../api/types'
import {
  POOL_SUGGESTIONS,
  contextCap,
  formatContext,
  isDuplicate,
  makePoolId,
  pickSuggestedModel,
  resolveSuggestion,
} from './executorPool'

const m = (id: string, extra: Partial<ModelInfo> = {}): ModelInfo => ({
  id,
  display_name: id,
  description: '',
  is_default: false,
  default_effort: null,
  efforts: ['low', 'medium', 'high', 'xhigh'],
  service_tiers: [],
  ...extra,
})

test('pickSuggestedModel：取版本號最大者，不因 API 順序而定', () => {
  const list = [m('claude-haiku-4-5'), m('claude-haiku-5-5'), m('claude-haiku-5-5[1m]')]
  assert.equal(pickSuggestedModel(POOL_SUGGESTIONS[0], list)?.id, 'claude-haiku-5-5')
  assert.equal(pickSuggestedModel(POOL_SUGGESTIONS[0], [m('haiku')])?.id, 'haiku')
  assert.equal(pickSuggestedModel(POOL_SUGGESTIONS[0], [m('sonnet')]), null)
})

test('pickSuggestedModel：agy 只收 Gemini 3.8+，優先 -low 檔', () => {
  const list = [m('gemini-3.8-flash-high'), m('gemini-3.8-flash-low'), m('gemini-2.5-flash-low'), m('gpt-6-luna')]
  assert.equal(pickSuggestedModel(POOL_SUGGESTIONS[4], list)?.id, 'gemini-3.8-flash-low')
})

test('contextCap：有 context_window 用它；claude 一律 1M；codex 後備 400k', () => {
  assert.equal(contextCap('claude', m('x', { context_window: 300000 }), []), 300000)
  assert.equal(contextCap('claude', m('claude-sonnet-5-5'), [m('claude-sonnet-5-5'), m('claude-sonnet-5-5[1m]')]), 1000000)
  assert.equal(contextCap('claude', m('claude-haiku-5-5'), []), 1000000)
  assert.equal(contextCap('claude', m('claude-x'), [m('claude-x')]), 1000000)
  assert.equal(contextCap('codex', m('gpt-6-luna'), []), 400000)
})

test('resolveSuggestion：建議的 context 不超過模型視窗；agy 沒有 context；模型沒有該強度就退成預設', () => {
  const list = [m('gpt-6.1-terra', { efforts: ['low', 'high'], context_window: 300000 })]
  const r = resolveSuggestion(POOL_SUGGESTIONS[2], list)
  assert.deepEqual(r, { kind: 'codex', model: 'gpt-6.1-terra', effort: null, context_tokens: 256000, count: 1 })
  const a = resolveSuggestion(POOL_SUGGESTIONS[4], [m('gemini-3.8-flash-low', { efforts: [] })])
  assert.deepEqual(a, { kind: 'agy', model: 'gemini-3.8-flash-low', effort: null, context_tokens: null, count: 2 })
  assert.equal(resolveSuggestion(POOL_SUGGESTIONS[3], []), null)
})

test('isDuplicate：四元組完全相同才算重複', () => {
  const pool = [{ id: 'a', kind: 'codex' as const, model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 1 }]
  assert.equal(isDuplicate(pool, { kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: 256000, count: 5 }), true)
  assert.equal(isDuplicate(pool, { kind: 'codex', model: 'gpt-6-luna', effort: 'xhigh', context_tokens: null, count: 1 }), false)
  assert.equal(isDuplicate(pool, { kind: 'codex', model: 'gpt-6-luna', effort: null, context_tokens: 256000, count: 1 }), false)
})

test('makePoolId：slug 且同專案內唯一', () => {
  const taken = new Set<string>()
  const e = { kind: 'codex' as const, model: 'gpt-6.1-terra', effort: 'medium', context_tokens: 256000, count: 1 }
  const first = makePoolId(e, taken)
  assert.equal(first, 'codex-gpt-6-1-terra-medium-256k')
  taken.add(first)
  assert.equal(makePoolId(e, taken), 'codex-gpt-6-1-terra-medium-256k-2')
  assert.equal(makePoolId({ ...e, context_tokens: 1000000 }, taken), 'codex-gpt-6-1-terra-medium-1m')
})

test('formatContext：k／M 排版', () => {
  assert.equal(formatContext(32000), '32k')
  assert.equal(formatContext(1000000), '1M')
})
