import test from 'node:test'
import assert from 'node:assert/strict'
import { modelMatches } from './ModelPicker.tsx'

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
