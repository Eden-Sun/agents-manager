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
