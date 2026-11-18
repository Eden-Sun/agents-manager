/** API kind 判斷（切片 E）：isApiKind／apiProvider 是唯一的集中判斷點。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { apiProvider, isApiKind, KIND_LABEL } from './kindMeta'
import { shortModel } from '../lib/shortModel'
import { parseModelSeries } from '../lib/modelSeries'
import { CREATABLE_KINDS, MODEL_OPTIONS } from '../api/types'

test('isApiKind：claude-api 與 openai-api 是 API kind；CLI kind 不是', () => {
  assert.equal(isApiKind('claude-api'), true)
  assert.equal(isApiKind('openai-api'), true)
  for (const k of ['claude', 'codex', 'grok', 'agy']) assert.equal(isApiKind(k), false, k)
  assert.equal(isApiKind(null), false)
  assert.equal(isApiKind(undefined), false)
})

test('apiProvider：依 kind 回供應商，非 API kind 回 null', () => {
  assert.equal(apiProvider('claude-api'), 'anthropic')
  assert.equal(apiProvider('openai-api'), 'openai')
  assert.equal(apiProvider('codex'), null)
})

test('openai-api 標籤是 OpenAI，可建立，模型只收 gpt-6-luna', () => {
  assert.equal(KIND_LABEL['openai-api'], 'OpenAI')
  assert.ok(CREATABLE_KINDS.includes('openai-api'))
  assert.deepEqual([...MODEL_OPTIONS['openai-api']], ['gpt-6-luna'])
})

test('shortModel：openai-api 留下名稱（gpt-6-luna → luna）', () => {
  assert.equal(shortModel('openai-api', 'gpt-6-luna'), 'luna')
  assert.equal(shortModel('openai-api', 'gpt-6.2-sol'), 'sol')
})

test('parseModelSeries：openai-api 的系列與 codex 分開', () => {
  assert.deepEqual(parseModelSeries('openai-api', 'gpt-6-luna'), { series: 'openai:luna', version: [6] })
  assert.deepEqual(parseModelSeries('codex', 'gpt-6-luna'), { series: 'codex:luna', version: [6] })
})
