import test from 'node:test'
import assert from 'node:assert/strict'
import { groupSendDelivered } from './groupSend.ts'

test('一顆都沒送到就是沒送出（草稿要留著）；有一顆送到就算送出', () => {
  assert.equal(groupSendDelivered({ delivered: false, sent: [] }), false)
  assert.equal(groupSendDelivered({ sent: [] }), false, '舊 daemon 沒有 delivered 欄，看 sent')
  assert.equal(groupSendDelivered({ delivered: true, sent: [{ bot_id: 'a', bot_name: 'a', turn_id: 't', message_id: 'm', delivery: 'ok' }] }), true)
  assert.equal(
    groupSendDelivered({ sent: [{ bot_id: 'a', bot_name: 'a', turn_id: 't', message_id: 'm', delivery: 'ok' }] }),
    true,
  )
})

import { groupSkipText } from './groupSend.ts'

test('跳過的收件者：機器碼翻成人話，toast 不貼英文原文', () => {
  assert.equal(groupSkipText({ reason: 'not_running', detail: 'bot has no active run' }), 'bot 未啟動（群組訊息不會自動啟動它）')
  assert.equal(groupSkipText({ reason: 'blocked', detail: 'agent is blocked; answer the prompt first' }), 'agent 正在等終端回應')
  assert.equal(groupSkipText({ reason: 'in_flight', detail: 'a turn is already in flight' }), '上一回合還在進行中')
  assert.equal(groupSkipText({ reason: 'unknown_delivery', detail: 'x' }), '上一回合送達狀態未知，請先放棄該回合')
  // 沒有固定說法的（conflict、upstream…）照抄 daemon 給的那一句；連那句都沒有就退回代碼，不留空括號。
  assert.equal(groupSkipText({ reason: 'conflict', detail: '維護窗口開著' }), '維護窗口開著')
  assert.equal(groupSkipText({ reason: 'upstream', detail: '' }), 'upstream')
})
