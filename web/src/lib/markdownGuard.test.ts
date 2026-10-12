import test from 'node:test'
import assert from 'node:assert/strict'
import { MARKDOWN_MAX_QUOTE_DEPTH, markdownTooDeep } from './markdownGuard'

test('引用層數超過上限就算太深（空白分隔）', () => {
  assert.equal(markdownTooDeep('> '.repeat(MARKDOWN_MAX_QUOTE_DEPTH + 1)), true)
  assert.equal(markdownTooDeep('> '.repeat(MARKDOWN_MAX_QUOTE_DEPTH)), false)
})

test('引用層數中間夾 tab 也要算：`>\\t` 重複會被 remark 解析成同樣的巢狀，不能繞過上限', () => {
  // remark 把 `>\t>` 解析成巢狀引用（實測 25 層）；守門若只認空白，這種輸入在 DOM 裡跑 87 秒。
  assert.equal(markdownTooDeep('>\t'.repeat(MARKDOWN_MAX_QUOTE_DEPTH + 1)), true)
  assert.equal(markdownTooDeep('>\t> '.repeat(MARKDOWN_MAX_QUOTE_DEPTH) + 'x'), true)
})
