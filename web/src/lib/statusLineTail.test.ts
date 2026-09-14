import { test } from 'node:test'
import assert from 'node:assert/strict'
import { trimClippedTail } from './statusLineTail.ts'

test('整個項目被砍掉時連分隔號一起去掉', () => {
  // 2026-09-14 使用者實拍：codex 在窄 pane 上把 weekly 那一項整個換成刪節號。
  assert.equal(
    trimClippedTail('gpt-6-astra high · ~/project · Context 57% used · 5h 96% left · …'),
    'gpt-6-astra high · ~/project · Context 57% used · 5h 96% left',
  )
})

test('只被砍一半的項目要留下它的數字', () => {
  assert.equal(
    trimClippedTail('gpt-6-astra medium · /tmp · Context 9% used · 5h 36% left · weekly 24%…'),
    'gpt-6-astra medium · /tmp · Context 9% used · 5h 36% left · weekly 24%',
  )
  assert.equal(trimClippedTail('… weekly 48% ...'), '… weekly 48%')
})

test('沒有刪節號就原樣回傳', () => {
  const full = 'gpt-5.6-sol high fast · /tmp · Context 0% used · 5h 82% left · weekly 73% left'
  assert.equal(trimClippedTail(full), full)
  assert.equal(trimClippedTail('hunta | pt | OP5 28% | 5h:98%'), 'hunta | pt | OP5 28% | 5h:98%')
  assert.equal(trimClippedTail(''), '')
})
