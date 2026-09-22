import { test } from 'node:test'
import assert from 'node:assert/strict'
import { identityBadgeVisible } from './identityBadgeVisible'

const ids = (...pairs: [string, string][]) => pairs.map(([name, kind]) => ({ name, kind: kind as 'claude' | 'codex' | 'grok' }))

test('那個 kind 只有一個帳號就不畫（codex：一個都沒有＝只有預設）', () => {
  assert.equal(identityBadgeVisible('codex', ids(['cc0', 'claude'], ['cc1', 'claude'], ['cc2', 'claude'])), false)
  assert.equal(identityBadgeVisible('grok', []), false)
})

test('claude 有好幾個帳號才畫', () => {
  assert.equal(identityBadgeVisible('claude', ids(['cc0', 'claude'], ['cc1', 'claude'])), true)
  // 只有一個具名身份時，「不指定」也是一種選擇，兩者分得出來才有意義 → 畫。
  assert.equal(identityBadgeVisible('claude', ids(['cc1', 'claude'])), true)
  // 一個都沒有＝只有預設一種 → 不畫。
  assert.equal(identityBadgeVisible('claude', []), false)
})

test('設定裡沒有、但 bot 身上綁著的身份照樣算數', () => {
  assert.equal(identityBadgeVisible('codex', [], ['gpt-alt']), true, '被刪掉的身份仍綁在 bot 上，要看得出跟別人不同')
  assert.equal(identityBadgeVisible('codex', [], [null, undefined, '']), false, '空的不算')
})
