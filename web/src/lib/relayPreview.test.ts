import test from 'node:test'
import assert from 'node:assert/strict'
import { RELAY_PREVIEW_MAX, relayPreview } from './relayPreview.ts'

test('多行交辦只露第一行', () => {
  const p = relayPreview('AGM 交辦：部署 origin/main 76d89c7\n\n流程照上次：乾淨 worktree\n回報 pid')
  assert.equal(p.text, 'AGM 交辦：部署 origin/main 76d89c7')
  assert.equal(p.truncated, true)
})

test('開頭空行跳過', () => {
  assert.equal(relayPreview('\n\n  AGM 裁示：好\n細節').text, 'AGM 裁示：好')
})

test('一行但太長就截斷，且不切半個字', () => {
  const long = '交'.repeat(RELAY_PREVIEW_MAX + 5)
  const p = relayPreview(long)
  assert.equal([...p.text].length, RELAY_PREVIEW_MAX + 1)
  assert.ok(p.text.endsWith('…'))
  assert.equal(p.truncated, true)
  assert.equal(p.length, RELAY_PREVIEW_MAX + 5)
})

test('短訊息不收合', () => {
  const p = relayPreview('AGM 裁示：可以')
  assert.equal(p.text, 'AGM 裁示：可以')
  assert.equal(p.truncated, false)
})
