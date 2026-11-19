import test from 'node:test'
import assert from 'node:assert/strict'
import {
  compareGptModels,
  formatGptModelLabel,
  formatGptShortModel,
  sortGptModelIds,
} from './gptModel'
import { shortModel } from './shortModel'

test('formatGptModelLabel：按鈕 label 推導（四個例子＋未知 id，由 id 推導不用 display_name）', () => {
  assert.equal(formatGptModelLabel('gpt-6-luna'), '6-Luna')
  assert.equal(formatGptModelLabel('gpt-6.1-terra'), '6.1-Terra')
  assert.equal(formatGptModelLabel('gpt-6.2-sol'), '6.2-Sol')
  assert.equal(formatGptModelLabel('gpt-6.2-astra'), '6.2-Astra')
  assert.equal(formatGptModelLabel('unknown-id'), 'unknown-id')
  assert.equal(formatGptModelLabel('custom-model-x'), 'custom-model-x')
})

test('sortGptModelIds：排序（亂序輸入 → luna, terra, sol, astra）', () => {
  assert.ok(compareGptModels('gpt-6-luna', 'gpt-6.1-terra') < 0)
  assert.ok(compareGptModels('gpt-6.1-terra', 'gpt-6.2-sol') < 0)
  assert.ok(compareGptModels('gpt-6.2-sol', 'gpt-6.2-astra') < 0)
  const input = ['gpt-6.2-astra', 'gpt-6.1-terra', 'gpt-6.2-sol', 'gpt-6-luna']
  const sorted = sortGptModelIds(input)
  assert.deepEqual(sorted, [
    'gpt-6-luna',
    'gpt-6.1-terra',
    'gpt-6.2-sol',
    'gpt-6.2-astra',
  ])
})

test('sortGptModelIds：同家族多版本依版本號由舊到新排序', () => {
  const input = [
    'gpt-6.2-sol',
    'gpt-6-sol',
    'gpt-6.1-terra',
    'gpt-6-luna',
    'gpt-5.6-luna',
    'gpt-6.2-astra',
    'gpt-6-astra',
  ]
  const sorted = sortGptModelIds(input)
  assert.deepEqual(sorted, [
    'gpt-5.6-luna',
    'gpt-6-luna',
    'gpt-6.1-terra',
    'gpt-6-sol',
    'gpt-6.2-sol',
    'gpt-6-astra',
    'gpt-6.2-astra',
  ])
})

test('shortModel：codex 與 openai-api 兩個 kind 一致回「<版本>-<名稱>」小寫', () => {
  assert.equal(formatGptShortModel('gpt-6-luna'), '6-luna')
  assert.equal(formatGptShortModel('gpt-6.2-astra'), '6.2-astra')
  const cases = [
    'gpt-6-luna',
    'gpt-6.1-terra',
    'gpt-6.2-sol',
    'gpt-6.2-astra',
    'gpt-6-luna-pro',
    'unknown-custom-model',
  ]
  for (const model of cases) {
    const codexResult = shortModel('codex', model)
    const openaiResult = shortModel('openai-api', model)
    assert.equal(
      codexResult,
      openaiResult,
      `kind codex 與 openai-api 對 ${model} 的 shortModel 結果必須一致`,
    )
  }
  assert.equal(shortModel('codex', 'gpt-6.2-astra'), '6.2-astra')
  assert.equal(shortModel('openai-api', 'gpt-6.2-astra'), '6.2-astra')
  assert.equal(shortModel('openai-api', 'gpt-6-luna'), '6-luna')
  assert.equal(shortModel('openai-api', 'gpt-6.1-terra'), '6.1-terra')
  assert.equal(shortModel('openai-api', 'gpt-6.2-sol'), '6.2-sol')
  assert.equal(shortModel('codex', null), null)
  assert.equal(shortModel('openai-api', null), null)
})
