import test from 'node:test'
import assert from 'node:assert/strict'
import { isMostlySimplified } from './zhSimplified.ts'

const SIMP = '这个问题我们需要修复一下，修复之后请再运行测试，确认没有问题了再告诉我们。'
const TRAD = '這個問題我們需要修復一下，修復之後請再執行測試，確認沒有問題了再告訴我們。'
const JA = '国会の議論を見て、会議の内容を社員に共有しました。ありがとうございます。'

test('簡體中文：是', () => {
  assert.equal(isMostlySimplified(SIMP), true)
  assert.equal(isMostlySimplified('修好了 parser 的 bug，这个问题我们需要修复一下，请再运行测试。'), true, '中英混雜、簡體為主')
})
test('繁體、日文、英文：否', () => {
  assert.equal(isMostlySimplified(TRAD), false)
  assert.equal(isMostlySimplified(JA), false)
  assert.equal(isMostlySimplified('I fixed the parser bug and re-ran every test in the suite.'), false)
})
test('程式碼區塊、行內 code、網址裡的簡體不算', () => {
  assert.equal(isMostlySimplified('```\n这个 这个 这个 这个\n```\n' + TRAD), false)
  assert.equal(isMostlySimplified('看 `这个 这个 这个` 與 https://example.com/这个/这个/这个 結果'), false)
})
