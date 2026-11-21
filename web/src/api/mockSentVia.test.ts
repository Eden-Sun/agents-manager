import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { useManualMockClock } from './mockClock.ts'
import { mockClaudeBot, mockMessages } from './mockBotHarness.ts'

before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()
const claudeBot = mockClaudeBot(mock)
const messages = mockMessages(mock)

test('補充帶 record：記成進行中回合的使用者訊息；不帶就不留', async () => {
  const id = await claudeBot()
  const out = (await mock.request('POST', `/bots/${id}/prompt`, { text: 'retry 跑久一點', client_request_id: 'sv-1' })) as { turn_id: string }
  const plain = (await mock.request('POST', `/bots/${id}/text`, { text: '1', enter: true })) as Record<string, unknown>
  assert.equal(plain.message_id, undefined)
  const rec = (await mock.request('POST', `/bots/${id}/text`, { text: '順便看 log', enter: true, record: true })) as { message_id: string | null }
  assert.ok(rec.message_id)
  const users = await messages(id, `?turn_id=${out.turn_id}&role=user`)
  assert.deepEqual(
    users.map((m) => [m.content, m.sent_via ?? null]),
    [['retry 跑久一點', null], ['順便看 log', 'supplement']],
  )
})

test('插隊真的打斷進行中的回合才標 send_now', async () => {
  const id = await claudeBot()
  await mock.request('POST', `/bots/${id}/prompt`, { text: 'retry 再跑一輪', client_request_id: 'sv-busy' }).catch(() => {})
  const out = (await mock.request('POST', `/bots/${id}/prompt`, { text: '改做這個', client_request_id: 'sv-2', send_now: true })) as { turn_id: string }
  const [m] = await messages(id, `?turn_id=${out.turn_id}&role=user`)
  assert.equal(m.sent_via, 'send_now')
})
