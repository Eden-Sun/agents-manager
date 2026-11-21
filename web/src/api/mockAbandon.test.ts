import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'
import { useManualMockClock } from './mockClock.ts'
import { mockClaudeBot, mockMessages } from './mockBotHarness.ts'

before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()
const claudeBot = mockClaudeBot(mock)
const messages = mockMessages(mock)

test('放棄一則已完成的回合：409 不改寫（daemon 只收 in-flight 或 delivery=unknown）', async () => {
  const id = await claudeBot()
  mock.simulateKeepWarmReply(id)
  const turnId = (await messages(id, '?role=user')).find((m) => m.keep_warm)?.turn_id
  assert.ok(turnId, '保溫回合要有 turn_id')
  await assert.rejects(
    mock.request('POST', `/turns/${turnId}/abandon`, {}),
    (e: unknown) => e instanceof ApiError && e.status === 409 && e.message === 'turn is neither in-flight nor of unknown delivery',
  )
})

test('放棄進行中的回合照常收：200 並標 failed', async () => {
  const id = await claudeBot()
  const out = (await mock.request('POST', `/bots/${id}/prompt`, { text: '放棄這一輪', client_request_id: 'ab-ok-1' })) as { turn_id: string }
  await mock.request('POST', `/turns/${out.turn_id}/abandon`, {})
  await assert.rejects(mock.request('POST', `/turns/${out.turn_id}/abandon`, {}), (e: unknown) => e instanceof ApiError && e.status === 409)
})
