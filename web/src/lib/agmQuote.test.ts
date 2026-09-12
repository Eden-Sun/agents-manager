import test from 'node:test'
import assert from 'node:assert/strict'
import { isAgmQuote } from './agmQuote.ts'

test('使用者轉述的總管訊息', () => {
  assert.equal(isAgmQuote('AGM 裁示：採你的提案 1'), true)
  assert.equal(isAgmQuote('AGM 回覆 3407527 重建申請：【核准，由你自己建】'), true)
  assert.equal(isAgmQuote('AGM 交辦（使用者指示）：把一條規則寫進 persona'), true)
  assert.equal(isAgmQuote('[AGM 轉交，來自 agents-manager-qn0ssg] 你排的那次重建'), true)
  assert.equal(isAgmQuote('  AG Man 補充：查到擁有者了'), true)
})

test('使用者自己的話不算', () => {
  assert.equal(isAgmQuote('問一下 AGM 這件事要不要做'), false)
  assert.equal(isAgmQuote('修 header 的 kind 文字'), false)
  // 開頭是 AGM 但第一行沒有冒號／方括號收尾＝在講 AGM，不是在轉述它。
  assert.equal(isAgmQuote('AGM 好像掛了，幫我看一下它的 health 然後回報給我知道'), false)
})
