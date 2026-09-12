import test from 'node:test'
import assert from 'node:assert/strict'
import { isAgmQuote, quotedFrom } from './agmQuote.ts'

test('使用者轉述的總管訊息', () => {
  assert.equal(quotedFrom('AGM 裁示：採你的提案 1'), 'AGM')
  assert.equal(quotedFrom('AGM 回覆 3407527 重建申請：【核准，由你自己建】'), 'AGM')
  assert.equal(quotedFrom('AGM 交辦（使用者指示）：把一條規則寫進 persona'), 'AGM')
  assert.equal(quotedFrom('[AGM 轉交，來自 agents-manager-qn0ssg] 你排的那次重建'), 'AGM')
  assert.equal(quotedFrom('  AG Man 補充：查到擁有者了'), 'AGM')
  assert.equal(isAgmQuote('AGM 裁示：好'), true)
})

test('bot 之間互轉的抬頭抓得到來源', () => {
  assert.equal(quotedFrom('[來自 c1-主要功能 / agents-manager-qn0ssg]'), 'c1-主要功能')
  assert.equal(quotedFrom('[來自 PT全] 這題我看過了'), 'PT全')
  assert.equal(isAgmQuote('[來自 c1-主要功能 / agents-manager-qn0ssg]'), false)
})

test('使用者自己的話不算', () => {
  assert.equal(quotedFrom('問一下 AGM 這件事要不要做'), null)
  assert.equal(quotedFrom('修 header 的 kind 文字'), null)
  assert.equal(quotedFrom('AGM 好像掛了，幫我看一下它的 health 然後回報給我知道'), null)
  // 「來自」不在開頭就不是抬頭。
  assert.equal(quotedFrom('這段是 [來自 c1-主要功能] 抄來的'), null)
})
