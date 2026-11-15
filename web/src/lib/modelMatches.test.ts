import test from 'node:test'
import assert from 'node:assert/strict'
import { modelMatches } from './modelMatches.ts'

// 2026-10-02：存的是 `claude-opus-5-5`（#400 把 `opus` 換成完整版號），按鈕是 `opus`，以前一顆都不亮。
test('完整版號對到系列別名的按鈕；別的系列、空值都不算', () => {
  assert.equal(modelMatches('opus', 'claude-opus-5-5'), true)
  assert.equal(modelMatches('sonnet', 'claude-sonnet-5-5'), true)
  assert.equal(modelMatches('opus', 'opus'), true)
  assert.equal(modelMatches('sonnet', 'claude-opus-5-5'), false)
  assert.equal(modelMatches('opus', null), false)
  // codex 的 id 自帶 `-`，只認完全相等。
  assert.equal(modelMatches('gpt-6.1-sol', 'gpt-6.1-sol'), true)
  assert.equal(modelMatches('gpt-6-sol', 'gpt-6.1-sol'), false)
})

test('等價別名雙向對稱比對、忽略大小寫與 context [1m] 變體', () => {
  // 雙向對稱
  assert.equal(modelMatches('claude-sonnet-5-5', 'sonnet'), true)
  assert.equal(modelMatches('claude-opus-5-5', 'opus'), true)
  assert.equal(modelMatches('claude-haiku-5-5', 'haiku'), true)
  assert.equal(modelMatches('claude-fable-5-1', 'fable'), true)
  // context 變體 [1m]
  assert.equal(modelMatches('sonnet', 'sonnet[1m]'), true)
  assert.equal(modelMatches('sonnet[1m]', 'sonnet'), true)
  assert.equal(modelMatches('sonnet[1m]', 'claude-sonnet-5-5'), true)
  assert.equal(modelMatches('claude-sonnet-5-5', 'sonnet[1m]'), true)
  assert.equal(modelMatches('claude-sonnet-5-5[1m]', 'sonnet'), true)
  assert.equal(modelMatches('claude-sonnet-5-5[1m]', 'claude-sonnet-5-5'), true)
  // 大小寫
  assert.equal(modelMatches('Sonnet', 'claude-sonnet-5-5'), true)
  assert.equal(modelMatches('GPT-6-Luna', 'gpt-6-luna'), true)
  // 真的不同家族或不同指定版號不相符
  assert.equal(modelMatches('sonnet', 'claude-opus-5-5'), false)
  assert.equal(modelMatches('claude-opus-4-6', 'claude-opus-5-5'), false)
  assert.equal(modelMatches('opus', 'sonnet[1m]'), false)
})
