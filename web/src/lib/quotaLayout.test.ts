import test from 'node:test'
import assert from 'node:assert/strict'
import { quotaCollapsed } from './quotaLayout.ts'

test('5 格放不進 1440 桌機的標題列（約 908px）：收合，不讓百分比疊到下一格的名字', () => {
  assert.equal(quotaCollapsed(908, 5), true)
})

test('900 寬視窗（沒有側欄）同樣 5 格：收合', () => {
  assert.equal(quotaCollapsed(900, 5), true)
})

test('寬螢幕（2560）5 格放得下：維持完整量表', () => {
  assert.equal(quotaCollapsed(1400, 5), false)
})

test('真 daemon 可能 cc0–cc6＋codex＋grok 共 9 格：需要更寬', () => {
  assert.equal(quotaCollapsed(1300, 9), true)
  assert.equal(quotaCollapsed(1400, 9), false)
})

test('格數少時沿用舊的 604 底線', () => {
  assert.equal(quotaCollapsed(600, 1), true)
  assert.equal(quotaCollapsed(700, 2), false)
})
