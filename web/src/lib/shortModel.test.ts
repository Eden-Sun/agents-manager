import test from 'node:test'
import assert from 'node:assert/strict'
import { agyEffort, shortModel } from './shortModel.ts'

test('claude 縮寫（含版號並去掉 [1m] 等上下文後綴）', () => {
  assert.equal(shortModel('claude', 'claude-opus-5-5'), 'opu-5.5')
  assert.equal(shortModel('claude', 'claude-opus-5-5[1m]'), 'opu-5.5')
  assert.equal(shortModel('claude', 'claude-sonnet-5-5'), 'son-5.5')
  assert.equal(shortModel('claude', 'claude-haiku-5-5'), 'hai-5.5')
  assert.equal(shortModel('claude', 'claude-fable-5-2'), 'fab-5.2')
  assert.equal(shortModel('claude', 'claude-fable-5-1'), 'fab-5.1')
  assert.equal(shortModel('claude', 'claude-opus-5'), 'opu-5')
  assert.equal(shortModel('claude', 'claude-sonnet-5-5-20260101'), 'son-5.5')
  assert.equal(shortModel('claude', 'opus'), 'opu')
  assert.equal(shortModel('claude', 'sonnet[1m]'), 'son')
  assert.equal(shortModel('claude', 'haiku[1m]'), 'hai')
  assert.equal(shortModel('claude', 'fable[1m]'), 'fab')
  assert.equal(shortModel('claude', 'claude-next-9'), 'claude-next-9')
})

test('codex 去掉每顆都一樣的 gpt- 前綴', () => {
  assert.equal(shortModel('codex', 'gpt-6-astra'), '6-astra')
  assert.equal(shortModel('codex', 'gpt-5.6-luna'), '5.6-luna')
  assert.equal(shortModel('codex', 'o4-gpt-mini'), 'o4-gpt-mini')
  assert.equal(shortModel('codex', null), null)
})

test('grok 不動', () => {
  assert.equal(shortModel('grok', 'grok-4.6'), 'grok-4.6')
})

test('agy 去掉 gemini-，尾巴的強度拆出去', () => {
  assert.equal(shortModel('agy', 'gemini-3.8-flash-high'), '3.8-flash')
  assert.equal(shortModel('agy', 'gemini-3.8-flash-medium'), '3.8-flash')
  assert.equal(shortModel('agy', 'gemini-4-pro'), '4-pro')
  assert.equal(agyEffort('agy', 'gemini-3.8-flash-high'), 'high')
  assert.equal(agyEffort('agy', 'gemini-4-pro'), null)
  assert.equal(agyEffort('claude', 'claude-x-high'), null)
})

test('openai-api 與 codex 規則統一，回 <版本>-<名稱> 小寫（使用者 2026-11-19）', () => {
  assert.equal(shortModel('openai-api', 'gpt-6-luna'), '6-luna')
  assert.equal(shortModel('openai-api', 'gpt-6.2-sol'), '6.2-sol')
  assert.equal(shortModel('openai-api', 'gpt-6-luna-pro'), '6-luna-pro')
  assert.equal(shortModel('openai-api', 'gpt-6.2-astra'), '6.2-astra')
  assert.equal(shortModel('openai-api', null), null)
  assert.equal(shortModel('codex', 'gpt-6-astra'), '6-astra')
  assert.equal(shortModel('codex', 'gpt-6.2-astra'), '6.2-astra')
  assert.equal(shortModel('claude-api', 'claude-opus-5-5'), 'opu-5.5')
})
