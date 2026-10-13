/**
 * mock 分享頁的回覆會把使用者那句的前 40 格原樣引回來；40 是 UTF-16 單位，截點落在 emoji 中間就會留下落單的代理字元。
 * 時間用手動時鐘推，不靠牆鐘。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { installManualClockTimers } from '../testing/manualClockTimers.ts'
import { mockShareClient } from './shareMock.ts'

const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/

test('mock 回覆引用使用者那句時，截在 emoji 中間也不留落單代理字元', async () => {
  const { clock, restore } = installManualClockTimers()
  try {
    const client = mockShareClient('demo_share_token_0123456789')
    const text = `${'a'.repeat(39)}😀後面還有字`
    const sent = client.send(text, 'crid-1', [])
    await clock.advance(3000)
    await sent
    const listed = client.messages()
    await clock.advance(200)
    const reply = (await listed).messages.at(-1)!
    assert.equal(reply.role, 'assistant')
    assert.doesNotMatch(reply.text, LONE_SURROGATE, JSON.stringify(reply.text))
  } finally {
    restore()
  }
})

test('mock 的字數上限以 code point 計（同 daemon 的 8000 字）：5000 個 emoji 不該被 413', async () => {
  const { clock, restore } = installManualClockTimers()
  try {
    const client = mockShareClient('demo_share_token_0123456789')
    const sent = client.send('😀'.repeat(5000), 'crid-2', [])
    await clock.advance(3000)
    await assert.doesNotReject(sent)
  } finally {
    restore()
  }
})
