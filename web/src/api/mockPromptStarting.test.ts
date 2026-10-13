/** mock 的 `start_if_stopped` 要跟 daemon 同形：run 還在 `starting` 也算「沒在跑」，先收下排隊、不 409（API.md §5，#122）。 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { useManualMockClock } from './mockClock.ts'

interface BotRow { id: string; kind: string }

// mock 的 bot 啟動是 timer：用虛擬時間，不在牆鐘上等它。
before(() => void useManualMockClock(true))
after(() => void useManualMockClock(false))
const mock = new MockTransport()
const bots = async () => ((await mock.request('GET', '/state')) as { projects: { bots: BotRow[] }[] }).projects.flatMap((p) => p.bots)

test('run 還在 starting 時送 start_if_stopped：200 排隊（delivery=queued），不 409', async () => {
  const b = (await bots()).find((x) => x.kind === 'claude')
  assert.ok(b, 'seed 要有 claude')
  // 啟動之後 timer 還沒跑：run 停在 starting。
  await mock.request('POST', `/bots/${b.id}/start`, {})
  const out = (await mock.request('POST', `/bots/${b.id}/prompt`, {
    text: '起來之後再送',
    client_request_id: 'starting-queue-1',
    start_if_stopped: true,
  })) as { delivery: string; turn_id: string }
  assert.equal(out.delivery, 'queued')
})
