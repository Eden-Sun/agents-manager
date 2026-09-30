import test from 'node:test'
import assert from 'node:assert/strict'
import { rawTransport, sendPrompt, withdrawTurn } from './index.ts'

test('sendPrompt can request daemon-owned queueing while a bot is busy', async () => {
  const request = rawTransport.request
  const calls: { method: string; path: string; body?: unknown }[] = []
  rawTransport.request = async (method, path, body) => {
    calls.push({ method, path, body })
    return { turn_id: 't1', message_id: 'm1', delivery: 'queued' }
  }
  try {
    const result = await sendPrompt('b1', '請跑測試', 'cr1', ['a1'], false, false, undefined, true)
    assert.deepEqual(result, { turn_id: 't1', message_id: 'm1', delivery: 'queued', send_now: null })
    assert.deepEqual(calls, [
      {
        method: 'POST',
        path: '/bots/b1/prompt',
        body: { text: '請跑測試', client_request_id: 'cr1', attachments: ['a1'], queue_if_busy: true },
      },
    ])
  } finally {
    rawTransport.request = request
  }
})

test('withdrawTurn returns the text and attachment IDs the composer must restore', async () => {
  const request = rawTransport.request
  rawTransport.request = async () => ({ text: '撤回文字', attachments: ['a1', 'a2'] })
  try {
    assert.deepEqual(await withdrawTurn('t1'), { text: '撤回文字', attachments: ['a1', 'a2'] })
  } finally {
    rawTransport.request = request
  }
})
