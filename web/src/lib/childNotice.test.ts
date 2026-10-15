import test from 'node:test'
import assert from 'node:assert/strict'
import { childNotice, childNoticeLabel, childNoticeTurns, groupChildNotices, isChildNoticeTurn } from './childNotice'
import { message, notice } from '../testing/childNoticeFixtures'
import { countUnreadTurns } from '../store/unread'

test('辨認固定開頭與 done/blocked 名字；使用者貼文、引文及未驗證 relay 不收', () => {
  assert.deepEqual(childNotice(notice('t')), { name: 'sc1779', status: 'done' })
  assert.deepEqual(childNotice(notice('t', 'child with spaces', true)), { name: 'child with spaces', status: 'blocked' })
  for (const extra of [{ relay_from: null }, { relay_unverified: true }, { turn_id: null }, { role: 'assistant' as const }, { content: '引述 ' + notice('t').content }]) {
    assert.equal(childNotice({ ...notice('t'), ...extra }), null)
  }
})

test('連續通知與同回合多則回覆合併，普通訊息中斷分組，孤立回覆不收', () => {
  const input = [notice('t1'), message('a1', 't1', 'assistant'), message('a2', 't1', 'assistant'), notice('t2', 'qs1780'),
    message('a3', 't2', 'assistant'), message('u', 'real', 'user'), message('a4', 'real', 'assistant'), notice('t3'), message('orphan', 'other', 'assistant')]
  const rows = groupChildNotices(input)
  assert.deepEqual(rows.map((r) => r.kind), ['notices', 'message', 'message', 'notices', 'message'])
  assert.equal(rows[0].kind === 'notices' && rows[0].messages.length, 5)
  assert.equal(rows[0].kind === 'notices' && childNoticeLabel(rows[0].notices), '↳ child sc1779、qs1780 回報完成（2）')
  assert.deepEqual(rows.flatMap((r) => r.kind === 'message' ? [r.message] : r.messages), input)
})

test('混合狀態保留兩個文案、同名去重但回合數不減', () => {
  assert.equal(childNoticeLabel([{ name: 'x', status: 'done' }, { name: 'x', status: 'done' }, { name: 'y', status: 'blocked' }]),
    '↳ child x 回報完成；child y 在等人回答（3）')
})

test('同回合有使用者補充時兩邊照舊顯示', () => {
  const input = [notice('t'), message('own', 't', 'user', '接著做'), message('answer', 't', 'assistant')]
  assert.equal(childNoticeTurns(input).size, 0)
  assert.equal(isChildNoticeTurn(input, 't', 'child-done:child:t'), false)
  assert.equal(countUnreadTurns(input, undefined), 1)
  assert.ok(groupChildNotices(input).every((r) => r.kind === 'message'))
})

test('未讀重算排除通知回合，普通與孤立回覆照算', () => {
  const input = [notice('t'), message('a', 't', 'assistant'), message('u', 'u', 'user'), message('b', 'u', 'assistant'), message('o', null, 'assistant')]
  assert.equal(countUnreadTurns(input, undefined), 2)
  assert.ok(isChildNoticeTurn(input, 't'))
  assert.ok(isChildNoticeTurn([], 'missing', 'child-done:child:t'))
  assert.ok(isChildNoticeTurn([], 'missing', 'child-blocked:child:t'))
  assert.equal(isChildNoticeTurn([], 'missing', 'web:child-done'), false)
})
