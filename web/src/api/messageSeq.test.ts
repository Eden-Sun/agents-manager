import test from 'node:test'
import assert from 'node:assert/strict'
import { toGroupMessagesPage, toMessage, toMessages } from './normalize.ts'

const T = '2026-10-01T10:00:00.123Z'
const raw = (id: string, seq?: number) => ({ id, conversation_id: 'c', role: 'user', content: id, source: 'web', created_at: T, ...(seq === undefined ? {} : { seq }) })

test('toMessage 帶出 daemon 的 seq；沒有或 0 就是 undefined（未知）', () => {
  assert.equal(toMessage(raw('a', 42))?.seq, 42)
  assert.equal(toMessage(raw('a'))?.seq, undefined)
  assert.equal(toMessage(raw('a', 0))?.seq, undefined)
})

test('toMessages／toGroupMessagesPage：同毫秒的訊息照 seq 排，不照 id', () => {
  const list = [raw('01M', 3), raw('01Z', 1), raw('01A', 2)]
  assert.deepEqual(toMessages(list, 'b').map((m) => m.id), ['01Z', '01A', '01M'])
  const group = toGroupMessagesPage({ messages: list.map((m) => ({ ...m, bot_id: 'b', bot_name: 'B' })), has_more: false }, 'p')
  assert.deepEqual(group.messages.map((m) => m.id), ['01Z', '01A', '01M'])
  // 舊 daemon 沒有 seq：照 id。
  assert.deepEqual(toMessages([raw('01M'), raw('01A')], 'b').map((m) => m.id), ['01A', '01M'])
})
