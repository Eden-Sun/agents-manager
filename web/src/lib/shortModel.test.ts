import test from 'node:test'
import assert from 'node:assert/strict'
import { shortModel } from './shortModel.ts'

test('claude 只留別名，認不得的原樣', () => {
  assert.equal(shortModel('claude', 'claude-fable-5-1'), 'fable')
  assert.equal(shortModel('claude', 'claude-opus-5'), 'opus')
  assert.equal(shortModel('claude', 'opus'), 'opus')
  assert.equal(shortModel('claude', 'claude-next-9'), 'claude-next-9')
})

test('codex 去掉每顆都一樣的 gpt- 前綴', () => {
  assert.equal(shortModel('codex', 'gpt-6-astra'), '6-astra')
  assert.equal(shortModel('codex', 'gpt-5.6-luna'), '5.6-luna')
  // 只去開頭那一段，名字中間的 gpt 不動。
  assert.equal(shortModel('codex', 'o4-gpt-mini'), 'o4-gpt-mini')
  assert.equal(shortModel('codex', null), null)
})

test('grok 不動', () => {
  assert.equal(shortModel('grok', 'grok-4.6'), 'grok-4.6')
})
