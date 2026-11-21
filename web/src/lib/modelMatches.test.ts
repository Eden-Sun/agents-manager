import test from 'node:test'
import assert from 'node:assert/strict'
import { modelMatches } from './modelMatches.ts'

test('完整版號對到系列別名的按鈕；別的系列、空值都不算', () => {
  assert.equal(modelMatches('opus', 'claude-opus-5-5'), true)
  assert.equal(modelMatches('sonnet', 'claude-sonnet-5-5'), true)
  assert.equal(modelMatches('opus', 'opus'), true)
  assert.equal(modelMatches('sonnet', 'claude-opus-5-5'), false)
  assert.equal(modelMatches('opus', null), false)
  assert.equal(modelMatches('gpt-6.1-sol', 'gpt-6.1-sol'), true)
  assert.equal(modelMatches('gpt-6-sol', 'gpt-6.1-sol'), false)
})

test('等價別名雙向對稱比對、忽略大小寫與 context [1m] 變體', () => {
  assert.equal(modelMatches('claude-sonnet-5-5', 'sonnet'), true)
  assert.equal(modelMatches('claude-opus-5-5', 'opus'), true)
  assert.equal(modelMatches('claude-haiku-5-5', 'haiku'), true)
  assert.equal(modelMatches('claude-fable-5-1', 'fable'), true)
  assert.equal(modelMatches('sonnet', 'sonnet[1m]'), true)
  assert.equal(modelMatches('sonnet[1m]', 'sonnet'), true)
  assert.equal(modelMatches('sonnet[1m]', 'claude-sonnet-5-5'), true)
  assert.equal(modelMatches('claude-sonnet-5-5', 'sonnet[1m]'), true)
  assert.equal(modelMatches('claude-sonnet-5-5[1m]', 'sonnet'), true)
  assert.equal(modelMatches('claude-sonnet-5-5[1m]', 'claude-sonnet-5-5'), true)
  assert.equal(modelMatches('Sonnet', 'claude-sonnet-5-5'), true)
  assert.equal(modelMatches('GPT-6-Luna', 'gpt-6-luna'), true)
  assert.equal(modelMatches('sonnet', 'claude-opus-5-5'), false)
  assert.equal(modelMatches('claude-opus-4-6', 'claude-opus-5-5'), false)
  assert.equal(modelMatches('opus', 'sonnet[1m]'), false)
})
