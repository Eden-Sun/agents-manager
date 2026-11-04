import test from 'node:test'
import assert from 'node:assert/strict'
import { isMostlyEnglish } from './isMostlyEnglish.ts'

test('英文段落：是', () => {
  assert.equal(isMostlyEnglish('I fixed the bug in the parser and added a regression test for it.'), true)
})
test('中文、中英夾雜但中文為主：否', () => {
  assert.equal(isMostlyEnglish('已經修好了，請重新整理頁面再試一次。'), false)
  assert.equal(isMostlyEnglish('我把 parser 的 bug 修好了，並且補了一個 regression test 放在 tests 目錄'), false)
})
test('太短、只有程式碼：否', () => {
  assert.equal(isMostlyEnglish('OK done'), false)
  assert.equal(isMostlyEnglish('```ts\nconst someVeryLongIdentifierName = computeSomethingExpensive(argumentsList)\n```'), false)
})
test('中文說明加大段英文程式碼區塊：看說明、不看程式碼', () => {
  assert.equal(isMostlyEnglish('結果如下：\n```\nthis is a long english log line that should not count at all here\n```'), false)
})
test('英文說明加一點中文專有名詞：是', () => {
  assert.equal(isMostlyEnglish('The daemon restarted cleanly and the 翻譯 button now shows up in every long English reply.'), true)
})
