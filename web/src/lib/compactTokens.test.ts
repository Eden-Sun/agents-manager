import test from 'node:test'
import assert from 'node:assert/strict'
import { compactTokens } from './composerCost.ts'

test('狀態列的 token 數：千取整寫 k，百萬寫 M', () => {
  assert.equal(compactTokens(168_800), '169k')
  assert.equal(compactTokens(999), '999')
  assert.equal(compactTokens(1_500_000), '1.5M')
  assert.equal(compactTokens(1_000_000), '1M')
})

test('千位進位滿 1000k 就換成 M：999600 寫 1M，不寫 1000k', () => {
  assert.equal(compactTokens(999_600), '1M')
})

test('百萬位的一位小數進位到 10 時寫整數：9.96M 寫 10M，不寫 10.0M', () => {
  assert.equal(compactTokens(9_960_000), '10M')
})
