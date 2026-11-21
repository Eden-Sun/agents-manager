import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { useManualMockClock } from './mockClock.ts'
import { startMockBot } from './mockBotHarness.ts'

before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()
const runningBot = (kind: string) => startMockBot(mock, kind)

test('grok 只改 effort：有在跑的 run 也要 needs_restart:true（不送 /effort）', async () => {
  const id = await runningBot('grok')
  const out = (await mock.request('PATCH', `/bots/${id}`, { effort: 'high' })) as { needs_restart: boolean }
  assert.equal(out.needs_restart, true)
})
