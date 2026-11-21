import test from 'node:test'
import assert from 'node:assert/strict'
import { apiProvider, isApiKind, KIND_LABEL } from './kindMeta'
import { shortModel } from '../lib/shortModel'
import { parseModelSeries } from '../lib/modelSeries'
import { CREATABLE_KINDS, MODEL_OPTIONS, openAiApiEffortsFor } from '../api/types'

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

test('openai-api 標籤是 OpenAI，可建立，核准模型 astra（預設）／sol／luna', () => {
  assert.equal(KIND_LABEL['openai-api'], 'OpenAI')
  assert.ok(CREATABLE_KINDS.includes('openai-api'))
  assert.deepEqual([...MODEL_OPTIONS['openai-api']], ['gpt-6-luna', 'gpt-6.2-sol', 'gpt-6.2-astra'])
  assert.equal(MODEL_OPTIONS['openai-api'][2], 'gpt-6.2-astra', '建立表單 astra 排最後（依家族便宜到貴）')
  assert.equal(openAiApiEffortsFor('gpt-6.2-astra').includes('none'), false, 'astra 不收 none')
  assert.equal(openAiApiEffortsFor('gpt-6-luna').includes('none'), true, 'luna 收 none')
})

test('shortModel：openai-api 留下版本與名稱（gpt-6.2-astra → 6.2-astra）', () => {
  assert.equal(shortModel('openai-api', 'gpt-6.2-astra'), '6.2-astra')
  assert.equal(shortModel('openai-api', 'gpt-6-luna'), '6-luna')
  assert.equal(shortModel('openai-api', 'gpt-6.2-sol'), '6.2-sol')
})

test('parseModelSeries：openai-api 的系列與 codex 分開', () => {
  assert.deepEqual(parseModelSeries('openai-api', 'gpt-6-luna'), { series: 'openai:luna', version: [6] })
  assert.deepEqual(parseModelSeries('codex', 'gpt-6-luna'), { series: 'codex:luna', version: [6] })
})
