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

test('daemon 實際會回的 422／403／503 跳過理由：toast 不出現機器碼，也不出現原始 JSON', () => {
  // daemon 的 `skip_reason`（daemon/src/group.rs）：detail 是 `human_of(body)`，沒有 message 時就是 body 裡的代碼
  // （例如 `prompt_too_long_to_prove`），或整個 JSON 字串。
  const shapes = [
    { reason: 'unprocessable', detail: 'prompt_too_long_to_prove' },
    { reason: 'unprocessable', detail: '{"error":"delivery_unprovable","reason":"prompt_too_long_to_prove","run_id":"r1","sent":false}' },
    { reason: 'forbidden', detail: 'relay_from_mismatch' },
    { reason: 'forbidden', detail: '{"error":"forbidden","reason":"relay_from_mismatch"}' },
    { reason: 'unavailable', detail: 'maintenance_state_unavailable' },
    { reason: 'unavailable', detail: '{"reason":"maintenance_state_unavailable","retryable":true,"sent":false}' },
    { reason: 'uncommitted', detail: 'run_state_not_recorded' },
    { reason: 'uncommitted', detail: '{"error":"run_state_not_recorded","retryable":true}' },
  ]
  for (const x of shapes) {
    const text = groupSkipText(x)
    assert.ok(text.length > 0, `${x.reason} 不能留空`)
    assert.doesNotMatch(text, /[{}[\]"]/, `${x.reason} 不能是原始 JSON：${text}`)
    assert.doesNotMatch(text, /[a-z]+_[a-z_]+|^[a-z_]+$/, `${x.reason} 不能是機器碼：${text}`)
  }
  assert.equal(groupSkipText({ reason: 'unprocessable', detail: 'prompt_too_long_to_prove' }), '這則訊息對方無法確認送達（多半是太長），請縮短後再送')
})
