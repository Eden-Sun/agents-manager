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

test('英文回覆內含一段繁中引用（約 6 行中文＋10 行英文）：是', () => {
  const englishLines = 'This is a test line.\n'.repeat(10)
  const chineseLines = '這是一段繁體中文的引用說明。\n'.repeat(6)
  assert.equal(isMostlyEnglish(englishLines + chineseLines), true)
})

test('英文回覆夾中文專有名詞（約 12%–16%，整段或逐行）：是', () => {
  const twelve = 'Please keep the 中信 table, the 保經 notes, and the 業務員報告書 wording in the branch, then rerun the checks and send the 錠嵂 result back to the lead.'
  const sixteen = 'The 中信 and 保經 notes plus the 業務員報告書 and KYC 招攬書 wording need one review before the batch ships to the lead, so keep them.'
  assert.equal(isMostlyEnglish(twelve.repeat(6)), true)
  assert.equal(isMostlyEnglish(Array(6).fill(sixteen).join('\n')), true)
})
test('繁中回覆夾英文指令／路徑：否', () => {
  assert.equal(isMostlyEnglish('已經修好了，請執行 `cd /home/ubuntu/project/am-wt-eng` 並確認狀態。'), false)
})

test('英文說明加多張數字表格（分隔列、數字列不算）：是', () => {
  const text = [
    'Here is the summary of the run, all checks passed on the branch.',
    '',
    '| 項目 | 數量 | 比例 |',
    '|---:|---|---|',
    '| 1 | | 18 | **55.6%** |',
    '| 2 | | 9 | **44.4%** |',
    '',
    'The remaining items are listed below for review.',
    '| 3 | | 7 | 12.0% |',
    '---',
  ].join('\n')
  assert.equal(isMostlyEnglish(text), true)
})
test('中文說明加數字表格：否', () => {
  const text = [
    '這次的結果如下，所有檢查都已通過。',
    '| 項目 | 數量 |',
    '|---|---|',
    '| 1 | 18 |',
    '| 2 | 9 |',
  ].join('\n')
  assert.equal(isMostlyEnglish(text), false)
})
test('純表格、無字母：否', () => {
  assert.equal(isMostlyEnglish('|---:|---|\n| 1 | | 18 |\n| 2 | | 9 |\n---'), false)
})
