import test from 'node:test'
import assert from 'node:assert/strict'
import { parseJevNotice } from './jevNotice'

test('「Jev 建議」開頭：拆出機率與正文', () => {
  assert.deepEqual(parseJevNotice('Jev 建議 82%：交辦 a1 的回報沒有驗證證據'), { prob: 0.82, rest: '交辦 a1 的回報沒有驗證證據' })
  assert.deepEqual(parseJevNotice('🤖 Jev 建議 5.5% 交辦 a1'), { prob: 0.055, rest: '交辦 a1' })
})

test('開頭沒寫機率：prob 是 null，正文照留', () => {
  assert.deepEqual(parseJevNotice('Jev 建議：請看一下'), { prob: null, rest: '請看一下' })
})

test('不是 Jev 開頭的訊息（含中間才提到 Jev）：null', () => {
  assert.equal(parseJevNotice('交辦 a1 的回報'), null)
  assert.equal(parseJevNotice('這是 Jev 建議 82% 的內容'), null)
  assert.equal(parseJevNotice('Jev 的看法'), null)
})

test('機率超過 100%：當作沒寫', () => {
  assert.equal(parseJevNotice('Jev 建議 250%：x')?.prob, null)
})

test('daemon 的完整開頭：括號整段吃掉，機率從括號裡拿', () => {
  assert.deepEqual(parseJevNotice('🤖 Jev 建議（TypeSafe 型別判斷，機率 73%，只是建議，不會自動動作）：交辦 a1 疑似撞題'), { prob: 0.73, rest: '交辦 a1 疑似撞題' })
  assert.deepEqual(parseJevNotice('🤖 Jev 建議（TypeSafe 型別判斷，只是建議，不會自動動作）：兩個機率在正文'), { prob: null, rest: '兩個機率在正文' })
})
