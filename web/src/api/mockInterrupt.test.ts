import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'
import { useManualMockClock } from './mockClock.ts'
import { mockClaudeBot } from './mockBotHarness.ts'

before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()
const claudeBot = mockClaudeBot(mock)

test('打斷帶的 turn_id 不在飛：409 turn_not_in_flight，在飛的那一筆不動', async () => {
  const id = await claudeBot()
  const out = (await mock.request('POST', `/bots/${id}/prompt`, { text: '打斷測試', client_request_id: 'int-1' })) as { turn_id: string }
  await assert.rejects(
    mock.request('POST', `/bots/${id}/interrupt`, { turn_id: 'turn_gone' }),
    (e: unknown) => e instanceof ApiError && e.status === 409 && e.message === 'turn_not_in_flight',
  )
  await mock.request('POST', `/turns/${out.turn_id}/abandon`, {})
})
