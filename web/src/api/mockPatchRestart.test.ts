/** mock 的 `PATCH /api/bots/{id}` 要跟 daemon 同形：grok 只改 effort 不當場套用，要 `needs_restart:true`（API.md §10.2、SPEC §4.4a：`/effort` 會寫進 grok 的 config.toml，daemon 不送）。 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { useManualMockClock } from './mockClock.ts'
import { startMockBot } from './mockBotHarness.ts'

// mock 的 bot 啟動是 timer：用虛擬時間，不在牆鐘上輪詢等它。
before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()
const runningBot = (kind: string) => startMockBot(mock, kind)

test('grok 只改 effort：有在跑的 run 也要 needs_restart:true（不送 /effort）', async () => {
  const id = await runningBot('grok')
  const out = (await mock.request('PATCH', `/bots/${id}`, { effort: 'high' })) as { needs_restart: boolean }
  assert.equal(out.needs_restart, true)
})
